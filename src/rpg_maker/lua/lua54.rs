//! 使用专用 OS worker 运行 RPG Maker 可信 Lua 5.4 的生产根适配器。

use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::io;
use std::num::{NonZeroU32, NonZeroUsize};
use std::os::windows::ffi::OsStrExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};

use futures_util::FutureExt;
use mlua::{
    AnyUserData, Function, HookTriggers, Lua, LuaOptions, MetaMethod, MultiValue, StdLib, Table,
    UserData, UserDataFields, UserDataMethods, Value, VmState,
};
use tokio::runtime::Handle;
use tokio::sync::{Notify, oneshot};

use crate::diagnostic::{
    DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact, DiagnosticReason,
    DiagnosticStage, DiagnosticSubject, RecoveryFact, SafeDiagnostic, SafeDiagnosticSource,
};
// 标准快照及其数据库存储分类仍由当前资产适配器提供；Lua VM 与宿主协议本身
// 已位于共享 RPG Maker 边界，不依赖具体引擎的命令编排。
use crate::fingerprint::{SHA256_FINGERPRINT_BYTES, Sha256Fingerprint};
use crate::llm::{ChatMessage, ChatMessageRole, LlmResponse, LlmUsage};
use crate::lossless_json::{LosslessJsonValue, decode as decode_json, validate_number};
use crate::rpg_maker::extract::store::{
    ExtractedTextGroup, ExtractedTextUnit, LuaSnapshot, SnapshotModelError,
};
use crate::rpg_maker::lua::document::{
    OpenedRpgMakerDocument, RpgMakerDocumentError, RpgMakerTextReference, data_file_source,
    data_source, map_source, plugin_parameter_source, source_path,
};
use crate::rpg_maker::lua::runtime::{
    OwnedLuaProgram, TrustedLuaBindingFinalizationError, TrustedLuaBindingFinalizer,
    TrustedLuaCommonBindings, TrustedLuaCommonHostCalls, TrustedLuaExecutionHandle,
    TrustedLuaExtractHostCalls, TrustedLuaHostCallError, TrustedLuaManagedTranslationCollection,
    TrustedLuaManagedTranslationCollectionDeclaration, TrustedLuaManagedTranslationContent,
    TrustedLuaManagedTranslationReport, TrustedLuaManagedTranslationResult,
    TrustedLuaManagedTranslationShape, TrustedLuaManagedTranslationSnapshot,
    TrustedLuaManagedTranslationUnit, TrustedLuaManagedTranslationUnitDeclaration,
    TrustedLuaPhaseBindings, TrustedLuaPreparedTranslation,
    TrustedLuaPreparedTranslationAcceptance, TrustedLuaRuntimeBindings,
    TrustedLuaRuntimeExecutionError, TrustedLuaRuntimeExecutionReport, TrustedLuaRuntimeExecutor,
    TrustedLuaStandardAcceptance, TrustedLuaStandardCandidate, TrustedLuaStandardHostCalls,
    TrustedLuaStandardRejectionValue, TrustedLuaStandardSession, TrustedLuaStandardUnit,
    TrustedLuaTranslateHostCalls, TrustedLuaWriteBackHostCalls, TrustedLuaWriteBackLayoutPair,
    TrustedLuaWriteBackLayoutRegion, TrustedLuaWriteBackLayoutResult,
};
use crate::rpg_maker::lua::{LuaPhase, LuaProjectContext, LuaSourcePath};
use crate::rpg_maker::model::{ScalarFieldKey, TextUnitContent, TextUnitRole};
use crate::rpg_maker::standard_asset::{
    RpgMakerStandardAssetOwner, validate_standard_text_locations,
};
use crate::rpg_maker::text::{
    RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource, TextGroupKind,
};
use crate::storage::file_system::ScopedDirectoryPath;
use crate::storage::sqlite::{SqliteCommand, SqliteQuery, SqliteRow, SqliteValue};

/// Lua worker 的内部执行策略。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TrustedLua54RuntimeConfiguration {
    worker_stack_bytes: NonZeroUsize,
    cancel_check_instruction_interval: NonZeroU32,
}

impl TrustedLua54RuntimeConfiguration {
    pub(crate) fn production() -> Self {
        Self::new(
            NonZeroUsize::new(8 * 1024 * 1024).expect("产品 worker 栈必须非零"),
            NonZeroU32::new(10_000).expect("取消检查间隔必须非零"),
        )
    }

    pub(crate) const fn new(
        worker_stack_bytes: NonZeroUsize,
        cancel_check_instruction_interval: NonZeroU32,
    ) -> Self {
        Self {
            worker_stack_bytes,
            cancel_check_instruction_interval,
        }
    }
}

/// Lua 生产根的线程启动、生命周期或 VM 失败。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TrustedLua54RuntimeError {
    WorkerSpawn {
        raw_os_code: Option<i32>,
    },
    ShuttingDown,
    WorkerChannelClosed,
    Vm {
        operation: &'static str,
        message: String,
    },
}

impl fmt::Display for TrustedLua54RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkerSpawn { raw_os_code } => match raw_os_code {
                Some(code) => write!(
                    formatter,
                    "无法创建 Lua worker：{}",
                    io::Error::from_raw_os_error(*code)
                ),
                None => formatter.write_str("无法创建 Lua worker"),
            },
            Self::ShuttingDown => formatter.write_str("Lua Runtime 正在关闭"),
            Self::WorkerChannelClosed => formatter.write_str("Lua worker 通道已关闭"),
            Self::Vm { message, .. } => formatter.write_str(message),
        }
    }
}

impl Error for TrustedLua54RuntimeError {}

impl SafeDiagnosticSource for TrustedLua54RuntimeError {
    fn safe_diagnostic_source(
        &self,
        stage: DiagnosticStage,
        impact: DiagnosticImpact,
        _fallback_action: DiagnosticAction,
    ) -> SafeDiagnostic {
        match self {
            Self::WorkerSpawn {
                raw_os_code: Some(code),
            } => SafeDiagnostic::io(
                DiagnosticCode::ShutdownComponent,
                stage,
                DiagnosticSubject::component("Lua worker"),
                "spawn_worker",
                &io::Error::from_raw_os_error(*code),
                impact,
                DiagnosticAction::Retry,
            ),
            Self::WorkerSpawn { raw_os_code: None } => SafeDiagnostic::new(
                DiagnosticCode::ShutdownComponent,
                stage,
                DiagnosticSubject::component("Lua worker"),
                DiagnosticReason::failure(DiagnosticFailureKind::WorkerSpawnFailed),
                impact,
                DiagnosticAction::Retry,
            ),
            Self::ShuttingDown => SafeDiagnostic::new(
                DiagnosticCode::ShutdownComponent,
                stage,
                DiagnosticSubject::component("Lua runtime"),
                DiagnosticReason::failure(DiagnosticFailureKind::ExecutorClosed),
                impact,
                DiagnosticAction::Retry,
            ),
            Self::WorkerChannelClosed => SafeDiagnostic::new(
                DiagnosticCode::ShutdownComponent,
                stage,
                DiagnosticSubject::component("Lua worker channel"),
                DiagnosticReason::failure(DiagnosticFailureKind::WorkerChannelClosed),
                impact,
                DiagnosticAction::ReportBug,
            ),
            Self::Vm { operation, .. } => SafeDiagnostic::new(
                DiagnosticCode::LuaExecution,
                stage,
                DiagnosticSubject::component("Lua VM"),
                DiagnosticReason::failure(DiagnosticFailureKind::LuaExecutionFailed),
                impact,
                DiagnosticAction::CheckProjectState,
            )
            .with_recovery(RecoveryFact::component(format!(
                "lua_vm_operation={operation}"
            ))),
        }
    }
}

struct RuntimeInner {
    lifecycle: Mutex<RuntimeLifecycle>,
    shutdown_requested: Arc<AtomicBool>,
    runtime_handles: AtomicUsize,
    jobs_finished: Notify,
    tokio: Handle,
    worker_stack_bytes: usize,
    cancel_check_instruction_interval: u32,
}

struct RuntimeLifecycle {
    accepting: bool,
    active_jobs: usize,
}

/// 进程内可信 Lua 5.4 Runtime。
///
/// VM 与允许的 Lua 标准库只在专用 OS worker 中存在。SQLite 与 LLM 调用通过同步
/// 响应桥交回构造本根的 Tokio Runtime 驱动。
pub(crate) struct TrustedLua54Runtime {
    inner: Arc<RuntimeInner>,
}

impl TrustedLua54Runtime {
    pub(crate) fn new(configuration: TrustedLua54RuntimeConfiguration, tokio: Handle) -> Self {
        let shutdown_requested = Arc::new(AtomicBool::new(false));

        Self {
            inner: Arc::new(RuntimeInner {
                lifecycle: Mutex::new(RuntimeLifecycle {
                    accepting: true,
                    active_jobs: 0,
                }),
                shutdown_requested,
                runtime_handles: AtomicUsize::new(1),
                jobs_finished: Notify::new(),
                tokio,
                worker_stack_bytes: configuration.worker_stack_bytes.get(),
                cancel_check_instruction_interval: configuration
                    .cancel_check_instruction_interval
                    .get(),
            }),
        }
    }

    /// 同步停止新任务并请求所有活动脚本合作取消。
    ///
    /// 该方法只发布取消事实，不等待 worker 或终结器；命令在取消等待闭包中调用后，
    /// 仍须调用 `shutdown` 回收所有资源。
    pub(crate) fn request_cancellation(&self) {
        self.inner.request_shutdown();
    }

    /// 停止新启动，取消正在执行的脚本，并等待 worker 与唯一终结器退出。
    ///
    /// 可信脚本进入 `os.execute` 或替换调试 hook 后可以长时间不交还控制；本方法
    /// 不伪造超时成功。
    pub(crate) async fn shutdown(&self) -> Result<(), TrustedLua54RuntimeError> {
        self.inner.request_shutdown();
        loop {
            let finished = self.inner.jobs_finished.notified();
            if self.inner.active_jobs() == 0 {
                break;
            }
            finished.await;
        }
        Ok(())
    }
}

impl RuntimeInner {
    fn request_shutdown(&self) {
        let mut lifecycle = self.lifecycle.lock().expect("Lua 生命周期锁不应中毒");
        lifecycle.accepting = false;
        self.shutdown_requested.store(true, Ordering::Release);
    }

    fn accept_job(&self) -> bool {
        let mut lifecycle = self.lifecycle.lock().expect("Lua 生命周期锁不应中毒");
        if !lifecycle.accepting {
            return false;
        }
        lifecycle.active_jobs = lifecycle
            .active_jobs
            .checked_add(1)
            .expect("Lua 活动 job 数不可能溢出");
        true
    }

    fn finish_job(&self) {
        let mut lifecycle = self.lifecycle.lock().expect("Lua 生命周期锁不应中毒");
        lifecycle.active_jobs = lifecycle
            .active_jobs
            .checked_sub(1)
            .expect("每个已接管 Lua job 只能完成一次");
        drop(lifecycle);
        self.jobs_finished.notify_waiters();
    }

    fn active_jobs(&self) -> usize {
        self.lifecycle
            .lock()
            .expect("Lua 生命周期锁不应中毒")
            .active_jobs
    }
}

impl Clone for TrustedLua54Runtime {
    fn clone(&self) -> Self {
        self.inner.runtime_handles.fetch_add(1, Ordering::AcqRel);
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Drop for TrustedLua54Runtime {
    fn drop(&mut self) {
        if self.inner.runtime_handles.fetch_sub(1, Ordering::AcqRel) == 1 {
            // 最后一个 Runtime 句柄只负责停止准入并请求合作取消；已经接管工作的
            // supervisor 仍拥有线程和唯一终结器，会自行完成收尾。
            self.inner.request_shutdown();
        }
    }
}

impl TrustedLuaRuntimeExecutor for TrustedLua54Runtime {
    type Error = TrustedLua54RuntimeError;

    fn start(
        &self,
        program: OwnedLuaProgram,
        bindings: TrustedLuaRuntimeBindings,
    ) -> TrustedLuaExecutionHandle<Self::Error> {
        let (common, phase, finalizer) = bindings.into_parts();
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation = RuntimeCancellation {
            local: Arc::clone(&cancelled),
            shutdown: Arc::clone(&self.inner.shutdown_requested),
        };
        let (worker_sender, worker_receiver) = oneshot::channel::<WorkerOutcome>();
        let (report_sender, report_receiver) = oneshot::channel();
        let accepted = self.inner.accept_job();
        let tokio = self.inner.tokio.clone();
        let policy = LuaExecutionPolicy {
            cancel_check_instruction_interval: self.inner.cancel_check_instruction_interval,
        };

        let worker = if accepted {
            let sender = Arc::new(Mutex::new(Some(worker_sender)));
            let worker_sender = Arc::clone(&sender);
            let worker = thread::Builder::new()
                .name("att-lua".to_owned())
                .stack_size(self.inner.worker_stack_bytes)
                .spawn(move || {
                    let outcome = catch_unwind(AssertUnwindSafe(|| {
                        execute_program(&program, common, phase, &tokio, &cancellation, policy)
                    }))
                    .unwrap_or(WorkerOutcome::Panicked);
                    if let Some(sender) = worker_sender
                        .lock()
                        .expect("Lua worker 结果锁不应中毒")
                        .take()
                    {
                        let _ = sender.send(outcome);
                    }
                });
            match worker {
                Ok(worker) => Some(worker),
                Err(error) => {
                    if let Some(sender) = sender.lock().expect("Lua worker 结果锁不应中毒").take()
                    {
                        let _ = sender.send(WorkerOutcome::Unavailable(
                            TrustedLua54RuntimeError::WorkerSpawn {
                                raw_os_code: error.raw_os_error(),
                            },
                        ));
                    }
                    None
                }
            }
        } else {
            let _ = worker_sender.send(WorkerOutcome::Unavailable(
                TrustedLua54RuntimeError::ShuttingDown,
            ));
            None
        };

        spawn_supervisor(
            Arc::clone(&self.inner),
            accepted,
            worker,
            worker_receiver,
            finalizer,
            report_sender,
        );

        TrustedLuaExecutionHandle::new(report_receiver, cancelled)
    }
}

fn spawn_supervisor(
    inner: Arc<RuntimeInner>,
    accepted: bool,
    worker: Option<JoinHandle<()>>,
    worker_receiver: oneshot::Receiver<WorkerOutcome>,
    finalizer: Box<dyn TrustedLuaBindingFinalizer>,
    report_sender: oneshot::Sender<TrustedLuaRuntimeExecutionReport<TrustedLua54RuntimeError>>,
) {
    let supervisor_runtime = Arc::clone(&inner);
    inner.tokio.spawn(async move {
        let mut runtime = match worker_receiver.await {
            Ok(result) => result.into_runtime_result(),
            Err(_) => Err(TrustedLuaRuntimeExecutionError::Unavailable(
                TrustedLua54RuntimeError::WorkerChannelClosed,
            )),
        };
        if let Some(worker) = worker {
            let joined = supervisor_runtime
                .tokio
                .spawn_blocking(move || worker.join())
                .await;
            if !matches!(joined, Ok(Ok(()))) {
                runtime = Err(TrustedLuaRuntimeExecutionError::WorkerPanicked);
            }
        }
        let finalization = match catch_unwind(AssertUnwindSafe(|| finalizer.finalize())) {
            Ok(finalization) => match AssertUnwindSafe(finalization).catch_unwind().await {
                Ok(finalization) => finalization,
                Err(_) => Err(TrustedLuaBindingFinalizationError::new(
                    "Lua Host 唯一终结器 panic",
                    None,
                )),
            },
            Err(_) => Err(TrustedLuaBindingFinalizationError::new(
                "Lua Host 唯一终结器 panic",
                None,
            )),
        };
        let _ = report_sender.send(TrustedLuaRuntimeExecutionReport::new(runtime, finalization));
        if accepted {
            supervisor_runtime.finish_job();
        }
    });
}

#[derive(Clone)]
struct RuntimeCancellation {
    local: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
}

#[derive(Clone, Copy)]
struct LuaExecutionPolicy {
    cancel_check_instruction_interval: u32,
}

impl RuntimeCancellation {
    fn is_cancelled(&self) -> bool {
        self.local.load(Ordering::Acquire) || self.shutdown.load(Ordering::Acquire)
    }
}

enum WorkerOutcome {
    Completed,
    Unavailable(TrustedLua54RuntimeError),
    Context(TrustedLua54RuntimeError),
    Compile(TrustedLua54RuntimeError),
    Execute(TrustedLua54RuntimeError),
    Binding(TrustedLuaHostCallError),
    Cancelled,
    Panicked,
}

impl WorkerOutcome {
    fn into_runtime_result(
        self,
    ) -> Result<(), TrustedLuaRuntimeExecutionError<TrustedLua54RuntimeError>> {
        match self {
            Self::Completed => Ok(()),
            Self::Unavailable(error) => Err(TrustedLuaRuntimeExecutionError::Unavailable(error)),
            Self::Context(error) => Err(TrustedLuaRuntimeExecutionError::Context(error)),
            Self::Compile(error) => Err(TrustedLuaRuntimeExecutionError::Compile(error)),
            Self::Execute(error) => Err(TrustedLuaRuntimeExecutionError::Execute(error)),
            Self::Binding(error) => Err(TrustedLuaRuntimeExecutionError::Binding(error)),
            Self::Cancelled => Err(TrustedLuaRuntimeExecutionError::Cancelled),
            Self::Panicked => Err(TrustedLuaRuntimeExecutionError::WorkerPanicked),
        }
    }
}

fn execute_program(
    program: &OwnedLuaProgram,
    common: TrustedLuaCommonBindings,
    phase: TrustedLuaPhaseBindings,
    tokio: &Handle,
    cancellation: &RuntimeCancellation,
    policy: LuaExecutionPolicy,
) -> WorkerOutcome {
    let LuaExecutionPolicy {
        cancel_check_instruction_interval,
    } = policy;
    if cancellation.is_cancelled() {
        return WorkerOutcome::Cancelled;
    }

    let script_path = safe_path_identity(program.main_script_path());
    let script_directory = script_directory(program.main_script_path());

    // SAFETY: 脚本是用户明确选择的完全可信本机程序；契约明确允许 debug、io、os
    // 与纯 Lua require。VM 只在当前专用 worker 线程中创建、使用和销毁。
    let lua = unsafe { Lua::unsafe_new_with(StdLib::ALL, LuaOptions::default()) };

    if let Err(error) = configure_module_paths(&lua, &script_directory) {
        return WorkerOutcome::Context(vm_error(
            "configure_module_paths",
            "无法配置 Lua package 路径",
            error,
        ));
    }
    let hook_cancellation = cancellation.clone();
    if let Err(error) = lua.set_hook(
        HookTriggers::new().every_nth_instruction(cancel_check_instruction_interval),
        move |_lua, _debug| {
            if hook_cancellation.is_cancelled() {
                Err(mlua::Error::runtime("ATT_LUA_CANCELLED"))
            } else {
                Ok(VmState::Continue)
            }
        },
    ) {
        return WorkerOutcome::Context(vm_error(
            "install_cancellation_hook",
            "无法安装 Lua 取消 hook",
            error,
        ));
    }
    if cancellation.is_cancelled() {
        return WorkerOutcome::Cancelled;
    }

    let context = match build_context(
        &lua,
        common,
        phase,
        program.main_script_path(),
        tokio.clone(),
        cancellation.clone(),
    ) {
        Ok(context) => context,
        Err(error) => {
            if cancellation.is_cancelled() {
                return WorkerOutcome::Cancelled;
            }
            return WorkerOutcome::Context(vm_error(
                "build_host_context",
                "无法构造 Lua ctx",
                error,
            ));
        }
    };
    if let Err(error) = lua.globals().set("ctx", context) {
        if cancellation.is_cancelled() {
            return WorkerOutcome::Cancelled;
        }
        return WorkerOutcome::Context(vm_error("inject_host_context", "无法注入 Lua ctx", error));
    }

    let function = match lua
        .load(program.source())
        .set_name(&script_path)
        .into_function()
    {
        Ok(function) => function,
        Err(error) => {
            if cancellation.is_cancelled() {
                return WorkerOutcome::Cancelled;
            }
            return WorkerOutcome::Compile(vm_error(
                "compile_main_program",
                "Lua 主程序编译失败",
                error,
            ));
        }
    };

    let runner: Function = match lua
        .load(
            "return function(main) local ok, value = xpcall(main, function(error) return error end); return ok, value end",
        )
        .eval()
    {
        Ok(runner) => runner,
        Err(error) => {
            if cancellation.is_cancelled() {
                return WorkerOutcome::Cancelled;
            }
            return WorkerOutcome::Context(vm_error(
                "build_execution_boundary",
                "无法构造 Lua 执行边界",
                error,
            ));
        }
    };

    let (succeeded, error): (bool, Value) = match runner.call(function) {
        Ok(result) => result,
        Err(error) => {
            if cancellation.is_cancelled() {
                return WorkerOutcome::Cancelled;
            }
            return WorkerOutcome::Execute(vm_error(
                "execute_main_program",
                "Lua 主程序运行失败",
                error,
            ));
        }
    };
    if cancellation.is_cancelled() {
        return WorkerOutcome::Cancelled;
    }
    if succeeded {
        return WorkerOutcome::Completed;
    }
    if let Value::UserData(userdata) = &error
        && let Ok(host_error) = userdata.borrow::<LuaHostErrorUserData>()
    {
        return WorkerOutcome::Binding(host_error.0.clone());
    }
    WorkerOutcome::Execute(TrustedLua54RuntimeError::Vm {
        operation: "execute_main_program",
        message: format!("Lua 主程序运行失败：{}", lua_value_description(&error)),
    })
}

/// 把 Windows 原始路径逐 UTF-16 code unit 编码成 Lua 可安全展示的无控制字符身份。
///
/// 该身份只用于 chunk 名、loader data 与诊断；真实模块查找始终使用原始 `PathBuf`。
fn safe_path_identity(path: &Path) -> String {
    use std::fmt::Write as _;

    let mut identity = String::from("@att-utf16");
    for unit in path.as_os_str().encode_wide() {
        write!(&mut identity, "-{unit:04X}").expect("写入 String 不会失败");
    }
    identity
}

fn script_directory(path: &Path) -> PathBuf {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

fn configure_module_paths(lua: &Lua, script_directory: &Path) -> mlua::Result<()> {
    let package: Table = lua.globals().get("package")?;
    install_unicode_lua_module_searcher(lua, &package, script_directory.to_path_buf())?;
    package.set("cpath", Value::Nil)?;
    package.set("loadlib", Value::Nil)
}

fn install_unicode_lua_module_searcher(
    lua: &Lua,
    package: &Table,
    script_directory: PathBuf,
) -> mlua::Result<()> {
    let lua_searcher = lua.create_function(move |lua, module: mlua::LuaString| {
        let module = strict_module_name(&module)?;
        let mut candidates = local_lua_module_candidates(&script_directory, &module);
        candidates.extend(package_lua_candidates(lua, &module)?);
        let mut diagnostics = String::new();
        for candidate in candidates {
            match std::fs::read(&candidate) {
                Ok(source) => {
                    let name = safe_path_identity(&candidate);
                    let loader = lua.load(source).set_name(&name).into_function()?;
                    return Ok(MultiValue::from_vec(vec![
                        Value::Function(loader),
                        Value::String(lua.create_string(&name)?),
                    ]));
                }
                Err(error) => {
                    use std::fmt::Write as _;
                    let name = safe_path_identity(&candidate);
                    let _ = write!(diagnostics, "\n\tno file '{name}' ({error})");
                }
            }
        }
        Ok(MultiValue::from_vec(vec![Value::String(
            lua.create_string(diagnostics)?,
        )]))
    })?;

    let current_searchers: Table = package.get("searchers")?;
    let preload: Value = current_searchers.raw_get(1)?;
    let searchers = lua.create_table()?;
    searchers.raw_set(1, preload)?;
    searchers.raw_set(2, lua_searcher)?;
    package.set("searchers", searchers)
}

fn strict_module_name(module: &mlua::LuaString) -> mlua::Result<String> {
    module
        .to_str()
        .map(|module| module.to_owned())
        .map_err(|_| mlua::Error::runtime("Lua 模块名不是 UTF-8"))
}

fn local_lua_module_candidates(script_directory: &Path, module: &str) -> Vec<PathBuf> {
    let module_path = PathBuf::from(module.replace('.', "\\"));
    let mut direct = script_directory.join(&module_path);
    direct.set_extension("lua");
    vec![direct, script_directory.join(module_path).join("init.lua")]
}

fn package_lua_candidates(lua: &Lua, module: &str) -> mlua::Result<Vec<PathBuf>> {
    let package: Table = lua.globals().get("package")?;
    let templates: mlua::LuaString = package.get("path")?;
    let templates = templates
        .to_str()
        .map_err(|_| mlua::Error::runtime("package.path 不是 UTF-8 字符串"))?;
    let module_path = module.replace('.', "\\");
    Ok(templates
        .split(';')
        .filter(|template| !template.is_empty())
        .map(|template| PathBuf::from(template.replace('?', &module_path)))
        .collect())
}

fn build_context(
    lua: &Lua,
    common: TrustedLuaCommonBindings,
    phase: TrustedLuaPhaseBindings,
    main_script_path: &Path,
    tokio: Handle,
    cancellation: RuntimeCancellation,
) -> mlua::Result<Table> {
    let phase_name = phase_name(phase.phase());
    let calls = Arc::clone(common.calls());
    let context = lua.create_table()?;
    // markers 以 JSON 容器 table 本身为键;弱键让脚本丢弃引用后的容器可被 GC
    // 回收,VM 峰值内存不随已处理文档数单调累积。仍被引用的容器键保持可达。
    let json_markers = lua.create_table()?;
    let markers_metatable = lua.create_table()?;
    markers_metatable.set("__mode", "k")?;
    json_markers.set_metatable(Some(markers_metatable))?;
    context.set("phase", phase_name)?;
    context.set("project", build_project_table(lua, calls.project())?)?;
    context.set("json", build_json_table(lua, &json_markers)?)?;
    context.set(
        "source",
        build_source_table(
            lua,
            Arc::clone(&calls),
            tokio.clone(),
            cancellation.clone(),
            &json_markers,
        )?,
    )?;
    context.set(
        "rpg_maker",
        build_rpg_maker_table(
            lua,
            Arc::clone(&calls),
            tokio.clone(),
            cancellation.clone(),
            &json_markers,
        )?,
    )?;
    context.set(
        "db",
        build_database_table(lua, Arc::clone(&calls), tokio.clone(), cancellation.clone())?,
    )?;
    match phase {
        TrustedLuaPhaseBindings::Extract(extract) => {
            install_extract_context(lua, &context, extract, &json_markers)?;
        }
        TrustedLuaPhaseBindings::Translate(translate) => {
            install_translate_context(
                lua,
                &context,
                translate,
                tokio,
                cancellation,
                &json_markers,
            )?;
        }
        TrustedLuaPhaseBindings::WriteBack(write_back) => {
            install_write_back_context(
                lua,
                &context,
                write_back,
                tokio,
                cancellation,
                &json_markers,
            )?;
        }
        TrustedLuaPhaseBindings::Project {
            arguments,
            standard,
        } => {
            install_project_context(lua, &context, standard, tokio, cancellation, &json_markers)?;
            install_project_arguments(lua, main_script_path, arguments)?;
        }
    }
    Ok(context)
}

fn install_extract_context(
    lua: &Lua,
    context: &Table,
    calls: Arc<dyn TrustedLuaExtractHostCalls>,
    json_markers: &Table,
) -> mlua::Result<()> {
    let extract = lua.create_table()?;
    let declared = Arc::new(AtomicBool::new(false));
    let replace_calls = Arc::clone(&calls);
    let replace_declared = Arc::clone(&declared);
    let replace = lua.create_function(move |lua, groups: Value| {
        let result = parse_lua_standard_snapshot(groups)
            .and_then(|snapshot| {
                claim_extract_intent(&replace_declared)?;
                replace_calls.replace_standard(snapshot)
            })
            .map_err(|error| error.with_operation("extract.replace_standard"));
        host_result_to_lua(lua, result, |_, ()| Ok(Value::Nil))
    })?;
    extract.set("replace_standard", checked_host_function(lua, replace)?)?;

    let clear_calls = Arc::clone(&calls);
    let clear = lua.create_function(move |lua, ()| {
        let result = claim_extract_intent(&declared)
            .and_then(|()| clear_calls.clear_standard())
            .map_err(|error| error.with_operation("extract.clear_standard"));
        host_result_to_lua(lua, result, |_, ()| Ok(Value::Nil))
    })?;
    extract.set("clear_standard", checked_host_function(lua, clear)?)?;
    context.set("extract", extract)?;
    install_extract_translations_context(lua, context, calls, json_markers)?;
    Ok(())
}

fn claim_extract_intent(declared: &AtomicBool) -> Result<(), TrustedLuaHostCallError> {
    declared
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| ())
        .map_err(|_| {
            TrustedLuaHostCallError::new(
                "extract",
                "intent_already_declared",
                "一次 Lua Extract 主程序只能声明一个标准快照意图",
                None,
                None,
            )
        })
}

fn parse_lua_standard_snapshot(value: Value) -> Result<LuaSnapshot, TrustedLuaHostCallError> {
    let Value::Table(groups) = value else {
        return Err(extract_argument_error(format!(
            "extract.replace_standard groups 必须是无洞数组，实际为 {}",
            value.type_name()
        )));
    };
    let groups = dense_values(groups, |error| extract_argument_error(error.to_string()))?
        .into_iter()
        .map(parse_lua_standard_group)
        .collect::<Result<Vec<_>, _>>()?;
    validate_unique_lua_standard_groups(&groups)?;
    LuaSnapshot::new(groups).map_err(snapshot_model_error)
}

fn validate_unique_lua_standard_groups(
    groups: &[ExtractedTextGroup],
) -> Result<(), TrustedLuaHostCallError> {
    let mut identities = HashSet::with_capacity(groups.len());
    for group in groups {
        if !identities.insert(group.group_location()) {
            return Err(extract_argument_error(format!(
                "extract.replace_standard groups 不允许重复的 group.location：{}",
                group.group_location()
            )));
        }
    }
    Ok(())
}

fn parse_lua_standard_group(value: Value) -> Result<ExtractedTextGroup, TrustedLuaHostCallError> {
    let Value::Table(group) = value else {
        return Err(extract_argument_error(format!(
            "extract.replace_standard 的每个 group 必须是 table，实际为 {}",
            value.type_name()
        )));
    };
    ensure_exact_string_keys(&group, &["kind", "location", "fields"])
        .map_err(|error| extract_argument_error(error.to_string()))?;
    let kind = parse_standard_group_kind(
        group
            .get("kind")
            .map_err(|error| extract_argument_error(error.to_string()))?,
    )?;
    let group_location = parse_lua_rpg_maker_location(
        group
            .get("location")
            .map_err(|error| extract_argument_error(error.to_string()))?,
        "group.location",
    )?;
    let fields: Value = group
        .get("fields")
        .map_err(|error| extract_argument_error(error.to_string()))?;
    let Value::Table(fields) = fields else {
        return Err(extract_argument_error(format!(
            "extract group.fields 必须是无洞数组，实际为 {}",
            fields.type_name()
        )));
    };
    let fields = dense_values(fields, |error| extract_argument_error(error.to_string()))?
        .into_iter()
        .map(|value| parse_lua_standard_field(value, kind, &group_location))
        .collect::<Result<Vec<_>, _>>()?;
    ExtractedTextGroup::new(kind, group_location, fields).map_err(snapshot_model_error)
}

fn parse_lua_standard_field(
    value: Value,
    kind: TextGroupKind,
    group_location: &RpgMakerLocation,
) -> Result<ExtractedTextUnit, TrustedLuaHostCallError> {
    let Value::Table(field) = value else {
        return Err(extract_argument_error(format!(
            "extract group.fields 的每一项必须是 table，实际为 {}",
            value.type_name()
        )));
    };
    ensure_exact_string_keys(&field, &["name", "text"])
        .map_err(|error| extract_argument_error(error.to_string()))?;
    let name = parse_rpg_maker_string(
        field
            .get("name")
            .map_err(|error| extract_argument_error(error.to_string()))?,
        "extract field.name",
    )
    .map_err(|error| extract_argument_error(error.to_string()))?;
    let text: Value = field
        .get("text")
        .map_err(|error| extract_argument_error(error.to_string()))?;
    let Value::UserData(text) = text else {
        return Err(extract_argument_error(format!(
            "extract field.text 必须是 RpgMakerDocument 建立的 RpgMakerTextRef，实际为 {}",
            text.type_name()
        )));
    };
    if !text.is::<LuaRpgMakerTextReference>() {
        return Err(extract_argument_error(
            "extract field.text 必须是 RpgMakerDocument 建立的 RpgMakerTextRef".to_owned(),
        ));
    }
    let text = text
        .borrow::<LuaRpgMakerTextReference>()
        .map_err(|error| extract_argument_error(error.to_string()))?
        .0
        .clone();
    validate_standard_text_locations(kind, text.location(), group_location)
        .map_err(|error| extract_argument_error(error.to_string()))?;
    ExtractedTextUnit::new_with_claim(
        name,
        text.location().clone(),
        text.mutation_claim().clone(),
        text.original().to_owned(),
    )
    .map_err(snapshot_model_error)
}

fn parse_lua_rpg_maker_location(
    value: Value,
    role: &str,
) -> Result<RpgMakerLocation, TrustedLuaHostCallError> {
    let Value::UserData(value) = value else {
        return Err(extract_argument_error(format!(
            "extract {role} 必须是 RpgMakerDocument 建立的位置，实际为 {}",
            value.type_name()
        )));
    };
    if !value.is::<LuaRpgMakerLocation>() {
        return Err(extract_argument_error(format!(
            "extract {role} 必须是 RpgMakerDocument 建立的位置"
        )));
    }
    value
        .borrow::<LuaRpgMakerLocation>()
        .map(|location| location.0.clone())
        .map_err(|error| extract_argument_error(error.to_string()))
}

fn parse_standard_group_kind(value: Value) -> Result<TextGroupKind, TrustedLuaHostCallError> {
    let kind = parse_rpg_maker_string(value, "extract group.kind")
        .map_err(|error| extract_argument_error(error.to_string()))?;
    match TextGroupKind::from_storage_name(&kind) {
        Some(
            kind @ (TextGroupKind::DatabaseEntry
            | TextGroupKind::System
            | TextGroupKind::Map
            | TextGroupKind::EventCommand
            | TextGroupKind::PluginParameter),
        ) => Ok(kind),
        _ => Err(extract_argument_error(format!(
            "extract group.kind 无效：{kind}"
        ))),
    }
}

fn snapshot_model_error(error: SnapshotModelError) -> TrustedLuaHostCallError {
    let message = error.to_string();
    TrustedLuaHostCallError::new(
        "extract",
        "invalid_standard_snapshot",
        message,
        None,
        Some(Arc::new(error)),
    )
}

fn extract_argument_error(message: String) -> TrustedLuaHostCallError {
    TrustedLuaHostCallError::new("extract", "invalid_standard_snapshot", message, None, None)
}

fn install_extract_translations_context(
    lua: &Lua,
    context: &Table,
    calls: Arc<dyn TrustedLuaExtractHostCalls>,
    json_markers: &Table,
) -> mlua::Result<()> {
    let translations = lua.create_table()?;
    let declared = Arc::new(AtomicBool::new(false));
    let markers = json_markers.clone();
    let native = lua.create_function(move |lua, arguments: MultiValue| {
        let result = claim_managed_once(
            &declared,
            "intent_already_declared",
            "一次 Lua Extract 主程序只能调用一次 translations.replace",
        )
        .and_then(|()| exact_managed_arguments(arguments, 1, "translations.replace"))
        .and_then(|mut arguments| parse_managed_translation_snapshot(arguments.remove(0), &markers))
        .and_then(|snapshot| calls.replace_managed(snapshot))
        .map_err(|error| error.with_operation("translations.replace"));
        host_result_to_lua(lua, result, |_, ()| Ok(Value::Nil))
    })?;
    translations.set("replace", checked_host_function(lua, native)?)?;
    context.set("translations", translations)
}

fn parse_managed_translation_snapshot(
    value: Value,
    json_markers: &Table,
) -> Result<TrustedLuaManagedTranslationSnapshot, TrustedLuaHostCallError> {
    let Value::Table(collections) = value else {
        return Err(managed_argument_error(format!(
            "translations.replace collections 必须是无洞数组，实际为 {}",
            value.type_name()
        )));
    };
    let values = dense_values(collections, |error| {
        managed_argument_error(format!(
            "translations.replace collections 必须是无洞数组：{error}"
        ))
    })?;
    let mut names = HashSet::with_capacity(values.len());
    let mut parsed = Vec::with_capacity(values.len());
    for (index, value) in values.into_iter().enumerate() {
        let collection = parse_managed_translation_collection(value, index + 1, json_markers)?;
        if !names.insert(collection.name().to_owned()) {
            return Err(managed_argument_error(format!(
                "translations.replace collections 不允许重复 name：{}",
                collection.name()
            )));
        }
        parsed.push(collection);
    }
    Ok(TrustedLuaManagedTranslationSnapshot::new(parsed))
}

fn parse_managed_translation_collection(
    value: Value,
    index: usize,
    json_markers: &Table,
) -> Result<TrustedLuaManagedTranslationCollectionDeclaration, TrustedLuaHostCallError> {
    let Value::Table(collection) = value else {
        return Err(managed_argument_error(format!(
            "translations.replace collections[{index}] 必须是 table，实际为 {}",
            value.type_name()
        )));
    };
    ensure_exact_string_keys(&collection, &["name", "instruction", "units"])
        .map_err(|error| managed_argument_error(error.to_string()))?;
    let name = managed_required_string(&collection, "name", &format!("collections[{index}].name"))?;
    if name.is_empty() {
        return Err(managed_argument_error(format!(
            "translations.replace collections[{index}].name 不能为空"
        )));
    }
    let instruction = managed_required_string(
        &collection,
        "instruction",
        &format!("collections[{index}].instruction"),
    )?;
    let units = collection
        .raw_get::<Value>("units")
        .map_err(|error| managed_argument_error(error.to_string()))?;
    let Value::Table(units) = units else {
        return Err(managed_argument_error(format!(
            "translations.replace collections[{index}].units 必须是无洞数组，实际为 {}",
            units.type_name()
        )));
    };
    let unit_values = dense_values(units, |error| {
        managed_argument_error(format!(
            "translations.replace collections[{index}].units 必须是无洞数组：{error}"
        ))
    })?;
    let mut keys = HashSet::with_capacity(unit_values.len());
    let mut parsed_units = Vec::with_capacity(unit_values.len());
    for (unit_offset, value) in unit_values.into_iter().enumerate() {
        let unit = parse_managed_translation_unit(value, index, unit_offset + 1, json_markers)?;
        if !keys.insert(unit.key().to_owned()) {
            return Err(managed_argument_error(format!(
                "translations.replace collection {name} 不允许重复 unit.key：{}",
                unit.key()
            )));
        }
        parsed_units.push(unit);
    }
    Ok(TrustedLuaManagedTranslationCollectionDeclaration::new(
        name,
        instruction,
        parsed_units,
    ))
}

fn parse_managed_translation_unit(
    value: Value,
    collection_index: usize,
    unit_index: usize,
    json_markers: &Table,
) -> Result<TrustedLuaManagedTranslationUnitDeclaration, TrustedLuaHostCallError> {
    let Value::Table(unit) = value else {
        return Err(managed_argument_error(format!(
            "translations.replace collections[{collection_index}].units[{unit_index}] 必须是 table，实际为 {}",
            value.type_name()
        )));
    };
    ensure_managed_unit_keys(&unit, collection_index, unit_index)?;
    let role = |field: &str| format!("collections[{collection_index}].units[{unit_index}].{field}");
    let key = managed_required_string(&unit, "key", &role("key"))?;
    if key.is_empty() {
        return Err(managed_argument_error(format!(
            "translations.replace {} 不能为空",
            role("key")
        )));
    }
    let kind = managed_required_string(&unit, "kind", &role("kind"))?;
    if kind.is_empty() {
        return Err(managed_argument_error(format!(
            "translations.replace {} 不能为空",
            role("kind")
        )));
    }
    let shape = parse_managed_shape(
        unit.raw_get::<Value>("shape")
            .map_err(|error| managed_argument_error(error.to_string()))?,
        &role("shape"),
    )?;
    let original = parse_managed_original(
        unit.raw_get::<Value>("original")
            .map_err(|error| managed_argument_error(error.to_string()))?,
        shape,
        &role("original"),
    )?;
    let context = managed_required_string(&unit, "context", &role("context"))?;
    let metadata = unit
        .raw_get::<Value>("metadata")
        .map_err(|error| managed_argument_error(error.to_string()))?;
    let metadata_json = match metadata {
        Value::Nil => None,
        value => Some(
            JsonEncoder::new(json_markers)
                .encode(value)
                .map_err(|error| {
                    managed_argument_error(format!(
                        "translations.replace {} 必须是可由 ctx.json 表达的 JSON 值：{error}",
                        role("metadata")
                    ))
                })?,
        ),
    };
    Ok(TrustedLuaManagedTranslationUnitDeclaration::new(
        key,
        kind,
        shape,
        original,
        context,
        metadata_json,
    ))
}

fn ensure_managed_unit_keys(
    unit: &Table,
    collection_index: usize,
    unit_index: usize,
) -> Result<(), TrustedLuaHostCallError> {
    let required = ["key", "kind", "shape", "original", "context"];
    let allowed = ["key", "kind", "shape", "original", "context", "metadata"];
    let mut found = HashSet::new();
    for pair in unit.clone().pairs::<Value, Value>() {
        let (key, _) = pair.map_err(|error| managed_argument_error(error.to_string()))?;
        let Value::String(key) = key else {
            return Err(managed_argument_error(format!(
                "translations.replace collections[{collection_index}].units[{unit_index}] 字段名必须是字符串"
            )));
        };
        let key = lua_string_to_text(&key, "managed unit 字段名")
            .map_err(|error| managed_argument_error(error.to_string()))?;
        if !allowed.contains(&key.as_str()) {
            return Err(managed_argument_error(format!(
                "translations.replace collections[{collection_index}].units[{unit_index}] 包含未知字段 {key}"
            )));
        }
        found.insert(key);
    }
    for field in required {
        if !found.contains(field) {
            return Err(managed_argument_error(format!(
                "translations.replace collections[{collection_index}].units[{unit_index}] 缺少字段 {field}"
            )));
        }
    }
    Ok(())
}

fn managed_required_string(
    table: &Table,
    field: &str,
    role: &str,
) -> Result<String, TrustedLuaHostCallError> {
    match table
        .raw_get::<Value>(field)
        .map_err(|error| managed_argument_error(error.to_string()))?
    {
        Value::String(value) => lua_string_to_text(&value, role)
            .map_err(|error| managed_argument_error(error.to_string())),
        value => Err(managed_argument_error(format!(
            "translations.replace {role} 必须是 UTF-8 字符串，实际为 {}",
            value.type_name()
        ))),
    }
}

fn parse_managed_shape(
    value: Value,
    role: &str,
) -> Result<TrustedLuaManagedTranslationShape, TrustedLuaHostCallError> {
    let Value::String(value) = value else {
        return Err(managed_argument_error(format!(
            "translations.replace {role} 必须是字符串，实际为 {}",
            value.type_name()
        )));
    };
    let value = lua_string_to_text(&value, role)
        .map_err(|error| managed_argument_error(error.to_string()))?;
    match value.as_str() {
        "single" => Ok(TrustedLuaManagedTranslationShape::Single),
        "reflow" => Ok(TrustedLuaManagedTranslationShape::Reflow),
        "lines" => Ok(TrustedLuaManagedTranslationShape::Lines),
        "items" => Ok(TrustedLuaManagedTranslationShape::Items),
        _ => Err(managed_argument_error(format!(
            "translations.replace {role} 无效：{value}"
        ))),
    }
}

fn parse_managed_original(
    value: Value,
    shape: TrustedLuaManagedTranslationShape,
    role: &str,
) -> Result<TrustedLuaManagedTranslationContent, TrustedLuaHostCallError> {
    match shape {
        TrustedLuaManagedTranslationShape::Single | TrustedLuaManagedTranslationShape::Reflow => {
            let Value::String(value) = value else {
                return Err(managed_argument_error(format!(
                    "translations.replace {role} 在 {} shape 下必须是 UTF-8 字符串，实际为 {}",
                    shape.as_str(),
                    value.type_name()
                )));
            };
            lua_string_to_text(&value, role)
                .map(TrustedLuaManagedTranslationContent::scalar)
                .map_err(|error| managed_argument_error(error.to_string()))
        }
        TrustedLuaManagedTranslationShape::Lines | TrustedLuaManagedTranslationShape::Items => {
            let Value::Table(values) = value else {
                return Err(managed_argument_error(format!(
                    "translations.replace {role} 在 {} shape 下必须是非空无洞字符串数组，实际为 {}",
                    shape.as_str(),
                    value.type_name()
                )));
            };
            let values = dense_values(values, |error| {
                managed_argument_error(format!(
                    "translations.replace {role} 必须是无洞字符串数组：{error}"
                ))
            })?;
            if values.is_empty() {
                return Err(managed_argument_error(format!(
                    "translations.replace {role} 在 {} shape 下不能为空数组",
                    shape.as_str()
                )));
            }
            let mut parsed = Vec::with_capacity(values.len());
            for (index, value) in values.into_iter().enumerate() {
                let Value::String(value) = value else {
                    return Err(managed_argument_error(format!(
                        "translations.replace {role}[{}] 必须是 UTF-8 字符串，实际为 {}",
                        index + 1,
                        value.type_name()
                    )));
                };
                let value = lua_string_to_text(&value, role)
                    .map_err(|error| managed_argument_error(error.to_string()))?;
                if shape == TrustedLuaManagedTranslationShape::Items && value.trim().is_empty() {
                    return Err(managed_argument_error(format!(
                        "translations.replace {role}[{}] 在 items shape 下不得为空白",
                        index + 1
                    )));
                }
                parsed.push(value);
            }
            Ok(TrustedLuaManagedTranslationContent::array(parsed))
        }
    }
}

fn exact_managed_arguments(
    arguments: MultiValue,
    expected: usize,
    operation: &str,
) -> Result<Vec<Value>, TrustedLuaHostCallError> {
    let arguments = arguments.into_vec();
    if arguments.len() != expected {
        return Err(managed_argument_error(format!(
            "{operation} 需要 {expected} 个参数，实际为 {} 个",
            arguments.len()
        )));
    }
    Ok(arguments)
}

fn claim_managed_once(
    claimed: &AtomicBool,
    kind: &'static str,
    message: &'static str,
) -> Result<(), TrustedLuaHostCallError> {
    claimed
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| ())
        .map_err(|_| TrustedLuaHostCallError::new("translations", kind, message, None, None))
}

fn managed_argument_error(message: String) -> TrustedLuaHostCallError {
    TrustedLuaHostCallError::new("translations", "invalid_snapshot", message, None, None)
}

fn install_translate_context(
    lua: &Lua,
    context: &Table,
    calls: Arc<dyn TrustedLuaTranslateHostCalls>,
    tokio: Handle,
    cancellation: RuntimeCancellation,
    json_markers: &Table,
) -> mlua::Result<()> {
    context.set(
        "translation",
        build_translation_table(lua, Arc::clone(&calls))?,
    )?;
    context.set(
        "llm",
        build_llm_function(lua, Arc::clone(&calls), tokio.clone(), cancellation.clone())?,
    )?;
    install_translate_translations_context(lua, context, calls, tokio, cancellation, json_markers)
}

fn install_translate_translations_context(
    lua: &Lua,
    context: &Table,
    calls: Arc<dyn TrustedLuaTranslateHostCalls>,
    tokio: Handle,
    cancellation: RuntimeCancellation,
    json_markers: &Table,
) -> mlua::Result<()> {
    let translations = lua.create_table()?;
    let translate_claimed = Arc::new(AtomicBool::new(false));
    let translate_succeeded = Arc::new(AtomicBool::new(false));

    let translate_calls = Arc::clone(&calls);
    let translate_tokio = tokio.clone();
    let translate_cancellation = cancellation.clone();
    let translate_markers = json_markers.clone();
    let claimed = Arc::clone(&translate_claimed);
    let succeeded = Arc::clone(&translate_succeeded);
    let native_translate = lua.create_function(move |lua, arguments: MultiValue| {
        let result = claim_managed_once(
            &claimed,
            "already_translated",
            "一次 Lua Translate 主程序只能调用一次 translations.translate",
        )
        .and_then(|()| exact_managed_arguments(arguments, 0, "translations.translate"))
        .and_then(|_| {
            wait_for_output_terminal(
                &translate_tokio,
                &translate_cancellation,
                translate_calls.translate_managed(),
            )
        })
        .inspect(|_| {
            succeeded.store(true, Ordering::Release);
        })
        .map_err(|error| error.with_operation("translations.translate"));
        host_result_to_lua(lua, result, |lua, report| {
            lua.create_userdata(LuaManagedTranslationReport {
                report,
                markers: translate_markers.clone(),
            })
            .map(Value::UserData)
        })
    })?;
    translations.set("translate", checked_host_function(lua, native_translate)?)?;

    let open_markers = json_markers.clone();
    let native_open = lua.create_function(move |lua, arguments: MultiValue| {
        let result = exact_managed_arguments(arguments, 1, "translations.open")
            .and_then(|mut arguments| parse_managed_open_name(arguments.remove(0)))
            .and_then(|name| {
                if !translate_succeeded.load(Ordering::Acquire) {
                    return Err(TrustedLuaHostCallError::new(
                        "translations",
                        "translate_required",
                        "Translate 阶段只能在本轮 translations.translate 成功后调用 translations.open",
                        None,
                        None,
                    ));
                }
                wait_for_host(&tokio, &cancellation, calls.open_managed(name))
            })
            .map_err(|error| error.with_operation("translations.open"));
        host_result_to_lua(lua, result, |lua, collection| match collection {
            Some(collection) => lua
                .create_userdata(LuaManagedTranslationCollection {
                    collection,
                    markers: open_markers.clone(),
                })
                .map(Value::UserData),
            None => Ok(Value::Nil),
        })
    })?;
    translations.set("open", checked_host_function(lua, native_open)?)?;
    context.set("translations", translations)
}

fn install_write_back_translations_context(
    lua: &Lua,
    context: &Table,
    calls: Arc<dyn TrustedLuaWriteBackHostCalls>,
    tokio: Handle,
    cancellation: RuntimeCancellation,
    json_markers: &Table,
) -> mlua::Result<()> {
    let translations = lua.create_table()?;
    let markers = json_markers.clone();
    let native = lua.create_function(move |lua, arguments: MultiValue| {
        let result = exact_managed_arguments(arguments, 1, "translations.open")
            .and_then(|mut arguments| parse_managed_open_name(arguments.remove(0)))
            .and_then(|name| wait_for_host(&tokio, &cancellation, calls.open_managed(name)))
            .map_err(|error| error.with_operation("translations.open"));
        host_result_to_lua(lua, result, |lua, collection| match collection {
            Some(collection) => lua
                .create_userdata(LuaManagedTranslationCollection {
                    collection,
                    markers: markers.clone(),
                })
                .map(Value::UserData),
            None => Ok(Value::Nil),
        })
    })?;
    translations.set("open", checked_host_function(lua, native)?)?;
    context.set("translations", translations)
}

fn parse_managed_open_name(value: Value) -> Result<String, TrustedLuaHostCallError> {
    let Value::String(value) = value else {
        return Err(TrustedLuaHostCallError::new(
            "translations",
            "invalid_argument",
            format!(
                "translations.open name 必须是非空 UTF-8 字符串，实际为 {}",
                value.type_name()
            ),
            None,
            None,
        ));
    };
    let value = lua_string_to_text(&value, "translations.open name")
        .map_err(|error| managed_open_argument_error(error.to_string()))?;
    if value.is_empty() {
        return Err(managed_open_argument_error(
            "translations.open name 不能为空".to_owned(),
        ));
    }
    Ok(value)
}

fn managed_open_argument_error(message: String) -> TrustedLuaHostCallError {
    TrustedLuaHostCallError::new("translations", "invalid_argument", message, None, None)
}

#[derive(Clone)]
struct LuaManagedTranslationReport {
    report: TrustedLuaManagedTranslationReport,
    markers: Table,
}

impl UserData for LuaManagedTranslationReport {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("units", |lua, this, ()| {
            let markers = this.markers.clone();
            let mut units = this.report.units().to_vec().into_iter();
            lua.create_function_mut(move |lua, _arguments: MultiValue| {
                let Some(result) = units.next() else {
                    return Ok(Value::Nil);
                };
                lua.create_userdata(LuaManagedTranslationResult {
                    result,
                    markers: markers.clone(),
                })
                .map(Value::UserData)
            })
        });
        methods.add_meta_method(MetaMethod::ToString, |_lua, _this, ()| {
            Ok("ManagedTranslationReport")
        });
    }
}

#[derive(Clone)]
struct LuaManagedTranslationResult {
    result: TrustedLuaManagedTranslationResult,
    markers: Table,
}

impl UserData for LuaManagedTranslationResult {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("collection", |_lua, this| {
            Ok(this.result.collection().to_owned())
        });
        fields.add_field_method_get("key", |_lua, this| Ok(this.result.key().to_owned()));
        fields.add_field_method_get("status", |_lua, this| Ok(this.result.status().as_str()));
        fields.add_field_method_get("translation", |lua, this| {
            managed_optional_content_to_lua(lua, this.result.translation(), &this.markers)
        });
        fields.add_field_method_get("reason", |_lua, this| {
            Ok(this.result.reason().map(str::to_owned))
        });
        fields.add_field_method_get("details", |lua, this| {
            managed_optional_json_object_to_lua(
                lua,
                this.result.details_json(),
                &this.markers,
                "translation result details",
            )
        });
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::ToString, |_lua, this, ()| {
            Ok(format!(
                "ManagedTranslationResult({}/{})",
                this.result.collection(),
                this.result.key()
            ))
        });
    }
}

#[derive(Clone)]
struct LuaManagedTranslationCollection {
    collection: TrustedLuaManagedTranslationCollection,
    markers: Table,
}

impl UserData for LuaManagedTranslationCollection {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("name", |_lua, this| Ok(this.collection.name().to_owned()));
        fields.add_field_method_get("instruction", |_lua, this| {
            Ok(this.collection.instruction().to_owned())
        });
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("get", |lua, this, key: Value| {
            let key = parse_managed_open_name(key)
                .map_err(|error| mlua::Error::external(error.message().to_owned()))?;
            match this.collection.get(&key) {
                Some(unit) => lua
                    .create_userdata(LuaManagedTranslationUnit {
                        unit: unit.clone(),
                        markers: this.markers.clone(),
                    })
                    .map(Value::UserData),
                None => Ok(Value::Nil),
            }
        });
        methods.add_method("units", |lua, this, ()| {
            let markers = this.markers.clone();
            let mut units = this.collection.units().to_vec().into_iter();
            lua.create_function_mut(move |lua, _arguments: MultiValue| {
                let Some(unit) = units.next() else {
                    return Ok(Value::Nil);
                };
                lua.create_userdata(LuaManagedTranslationUnit {
                    unit,
                    markers: markers.clone(),
                })
                .map(Value::UserData)
            })
        });
        methods.add_meta_method(MetaMethod::ToString, |_lua, this, ()| {
            Ok(format!(
                "ManagedTranslationCollection({})",
                this.collection.name()
            ))
        });
    }
}

#[derive(Clone)]
struct LuaManagedTranslationUnit {
    unit: TrustedLuaManagedTranslationUnit,
    markers: Table,
}

impl UserData for LuaManagedTranslationUnit {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("key", |_lua, this| Ok(this.unit.key().to_owned()));
        fields.add_field_method_get("kind", |_lua, this| Ok(this.unit.kind().to_owned()));
        fields.add_field_method_get("shape", |_lua, this| Ok(this.unit.shape().as_str()));
        fields.add_field_method_get("original", |lua, this| {
            managed_content_to_lua(lua, this.unit.original(), &this.markers)
        });
        fields.add_field_method_get("context", |_lua, this| Ok(this.unit.context().to_owned()));
        fields.add_field_method_get("metadata", |lua, this| {
            managed_optional_json_to_lua(
                lua,
                this.unit.metadata_json(),
                &this.markers,
                "unit metadata",
            )
        });
        fields.add_field_method_get("translation", |lua, this| {
            managed_optional_content_to_lua(lua, this.unit.translation(), &this.markers)
        });
        fields.add_field_method_get("status", |_lua, this| Ok(this.unit.status().as_str()));
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::ToString, |_lua, this, ()| {
            Ok(format!("ManagedTranslationUnit({})", this.unit.key()))
        });
    }
}

fn managed_optional_content_to_lua(
    lua: &Lua,
    content: Option<&TrustedLuaManagedTranslationContent>,
    markers: &Table,
) -> mlua::Result<Value> {
    match content {
        Some(content) => managed_content_to_lua(lua, content, markers),
        None => Ok(Value::Nil),
    }
}

fn managed_content_to_lua(
    lua: &Lua,
    content: &TrustedLuaManagedTranslationContent,
    markers: &Table,
) -> mlua::Result<Value> {
    match content {
        TrustedLuaManagedTranslationContent::Scalar(value) => {
            lua.create_string(value).map(Value::String)
        }
        TrustedLuaManagedTranslationContent::Array(values) => {
            let output = lua.create_table_with_capacity(values.len(), 0)?;
            for (index, value) in values.iter().enumerate() {
                output.raw_set(index + 1, value.as_str())?;
            }
            mark_json_table(markers, &output, JsonContainerKind::Array)?;
            Ok(Value::Table(output))
        }
    }
}

fn managed_optional_json_object_to_lua(
    lua: &Lua,
    source: Option<&str>,
    markers: &Table,
    role: &str,
) -> mlua::Result<Value> {
    let Some(source) = source else {
        return Ok(Value::Nil);
    };
    let value = decode_json(source).map_err(|error| {
        mlua::Error::runtime(format!("托管核心返回了无效 {role} JSON：{error}"))
    })?;
    if !matches!(value, LosslessJsonValue::Object(_)) {
        return Err(mlua::Error::runtime(format!(
            "托管核心返回的 {role} 必须是 JSON object"
        )));
    }
    lossless_json_to_lua(lua, value, markers)
}

fn managed_optional_json_to_lua(
    lua: &Lua,
    source: Option<&str>,
    markers: &Table,
    role: &str,
) -> mlua::Result<Value> {
    let Some(source) = source else {
        return Ok(Value::Nil);
    };
    let value = decode_json(source).map_err(|error| {
        mlua::Error::runtime(format!("托管核心返回了无效 {role} JSON：{error}"))
    })?;
    lossless_json_to_lua(lua, value, markers)
}

fn install_project_context(
    lua: &Lua,
    context: &Table,
    calls: Arc<dyn TrustedLuaStandardHostCalls>,
    tokio: Handle,
    cancellation: RuntimeCancellation,
    json_markers: &Table,
) -> mlua::Result<()> {
    let standard = lua.create_table()?;
    let markers = json_markers.clone();
    let native = lua.create_function(move |lua, ()| {
        let result = wait_for_host(&tokio, &cancellation, calls.open())
            .map_err(|error| error.with_operation("standard.open"));
        host_result_to_lua(lua, result, |lua, session| {
            lua.create_userdata(LuaStandardSession {
                identity: Arc::new(()),
                session,
                tokio: tokio.clone(),
                cancellation: cancellation.clone(),
                markers: markers.clone(),
            })
            .map(Value::UserData)
        })
    })?;
    standard.set("open", checked_host_function(lua, native)?)?;
    context.set("standard", standard)
}

fn install_project_arguments(
    lua: &Lua,
    main_script_path: &Path,
    arguments: Vec<String>,
) -> mlua::Result<()> {
    let values = lua.create_table()?;
    values.raw_set(0, strict_path(main_script_path)?)?;
    for (index, value) in arguments.into_iter().enumerate() {
        values.raw_set(index + 1, value)?;
    }
    lua.globals().set("arg", values)
}

struct LuaStandardSession {
    identity: Arc<()>,
    session: Arc<dyn TrustedLuaStandardSession>,
    tokio: Handle,
    cancellation: RuntimeCancellation,
    markers: Table,
}

impl UserData for LuaStandardSession {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("units", |lua, this| {
            let identity = Arc::clone(&this.identity);
            let markers = this.markers.clone();
            let session = Arc::clone(&this.session);
            let native = lua.create_function(move |lua, _session: Value| {
                let identity = Arc::clone(&identity);
                let markers = markers.clone();
                let mut units = session.units().into_iter();
                let iterator = lua.create_function_mut(move |lua, _arguments: MultiValue| {
                    let Some(unit) = units.next() else {
                        return Ok(Value::Nil);
                    };
                    lua.create_userdata(LuaStandardUnit {
                        identity: Arc::clone(&identity),
                        unit,
                        markers: markers.clone(),
                    })
                    .map(Value::UserData)
                })?;
                Ok(MultiValue::from_vec(vec![
                    Value::Boolean(true),
                    Value::Function(iterator),
                ]))
            })?;
            checked_host_function(lua, native)
        });

        fields.add_field_method_get("get", |lua, this| {
            let identity = Arc::clone(&this.identity);
            let session = Arc::clone(&this.session);
            let markers = this.markers.clone();
            let native = lua.create_function(
                move |lua,
                      (_session, owner, location, role): (Value, Value, Value, Value)| {
                    let result = parse_standard_owner(owner)
                        .and_then(|owner| {
                            let Value::UserData(location) = location else {
                                return Err(standard_argument_error(format!(
                                    "Standard group_location 必须是 RpgMakerLocation userdata，实际为 {}",
                                    location.type_name()
                                )));
                            };
                            if !location.is::<LuaRpgMakerLocation>() {
                                return Err(standard_argument_error(
                                    "Standard group_location 只接受 Rust 建立的 RpgMakerLocation"
                                        .to_owned(),
                                ));
                            }
                            let location = location
                                .borrow::<LuaRpgMakerLocation>()
                                .map(|location| location.0.clone())
                                .map_err(binding_error)?;
                            let role = parse_standard_role(role)?;
                            Ok(session.get(owner, location, role))
                        })
                        .map_err(|error| error.with_operation("standard.get"));
                    host_result_to_lua(lua, result, |lua, unit| match unit {
                        Some(unit) => lua
                            .create_userdata(LuaStandardUnit {
                                identity: Arc::clone(&identity),
                                unit,
                                markers: markers.clone(),
                            })
                            .map(Value::UserData),
                        None => Ok(Value::Nil),
                    })
                },
            )?;
            checked_host_function(lua, native)
        });

        fields.add_field_method_get("accept", |lua, this| {
            let identity = Arc::clone(&this.identity);
            let session = Arc::clone(&this.session);
            let tokio = this.tokio.clone();
            let cancellation = this.cancellation.clone();
            let markers = this.markers.clone();
            let native = lua.create_function(move |lua, (_session, batch): (Value, Value)| {
                let candidates = parse_standard_candidate_batch(batch, &identity)
                    .map_err(|error| error.with_operation("standard.accept"));
                let result = match candidates {
                    Ok(candidates) => {
                        let expected_results = candidates.len();
                        wait_for_output_terminal(&tokio, &cancellation, session.accept(candidates))
                            .and_then(|results| {
                                if results.len() == expected_results {
                                    Ok(results)
                                } else {
                                    Err(TrustedLuaHostCallError::new(
                                        "standard",
                                        "invalid_result",
                                        "Standard 核心返回的验收结果数量与候选数量不一致",
                                        None,
                                        None,
                                    )
                                    .with_operation("standard.accept"))
                                }
                            })
                    }
                    Err(error) => Err(error),
                };
                host_result_to_lua(lua, result, |lua, results| {
                    standard_acceptances_to_lua(lua, results, &markers)
                })
            })?;
            checked_host_function(lua, native)
        });
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::ToString, |_lua, _this, ()| {
            Ok("StandardSession")
        });
    }
}

#[derive(Clone)]
struct LuaStandardUnit {
    identity: Arc<()>,
    unit: TrustedLuaStandardUnit,
    markers: Table,
}

impl UserData for LuaStandardUnit {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("owner", |_lua, this| Ok(this.unit.owner().storage_name()));
        fields.add_field_method_get("group_kind", |_lua, this| {
            Ok(standard_group_kind_name(this.unit.group_kind()))
        });
        fields.add_field_method_get("group_location", |lua, this| {
            lua.create_userdata(LuaRpgMakerLocation(this.unit.group_location().clone()))
        });
        fields.add_field_method_get("role", |lua, this| {
            standard_role_to_lua(lua, this.unit.role(), &this.markers)
        });
        fields.add_field_method_get("original", |lua, this| {
            standard_content_to_lua(lua, this.unit.original(), &this.markers)
        });
        fields.add_field_method_get("source_context", |lua, this| {
            let context = decode_json(this.unit.source_context_json()).map_err(|error| {
                mlua::Error::runtime(format!(
                    "Standard 核心返回了无效 source_context JSON：{error}"
                ))
            })?;
            lossless_json_to_lua(lua, context, &this.markers)
        });
        fields.add_field_method_get("translation", |lua, this| match this.unit.translation() {
            Some(translation) => standard_content_to_lua(lua, translation, &this.markers),
            None => Ok(Value::Nil),
        });
        fields.add_field_method_get("model_text", |lua, this| {
            standard_content_to_lua(lua, this.unit.model_text(), &this.markers)
        });
        fields.add_field_method_get("terms", |lua, this| {
            let terms = lua.create_table()?;
            for (index, term) in this.unit.terms().iter().enumerate() {
                let entry = lua.create_table()?;
                entry.set("term", term.term())?;
                entry.set("translation", term.translation())?;
                mark_json_table(&this.markers, &entry, JsonContainerKind::Object)?;
                terms.raw_set(index + 1, entry)?;
            }
            mark_json_table(&this.markers, &terms, JsonContainerKind::Array)?;
            Ok(terms)
        });
        fields.add_field_method_get("content_kind", |_lua, this| {
            Ok(this.unit.content_kind().as_str())
        });
        fields.add_field_method_get("line_policy", |_lua, this| {
            Ok(this.unit.line_policy().as_str())
        });
        fields.add_field_method_get("expected_line_count", |_lua, this| {
            this.unit
                .line_policy()
                .expected_line_count()
                .map(usize_to_lua_integer)
                .transpose()
        });
        fields.add_field_method_get("status", |_lua, this| Ok(this.unit.status().as_str()));
        fields.add_field_method_get("family_size", |_lua, this| {
            usize_to_lua_integer(this.unit.family_size())
        });
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::ToString, |_lua, this, ()| {
            Ok(format!(
                "StandardUnit({}/{}/{})",
                this.unit.owner().storage_name(),
                this.unit.group_location(),
                standard_role_name(this.unit.role())
            ))
        });
    }
}

fn parse_standard_owner(
    value: Value,
) -> Result<RpgMakerStandardAssetOwner, TrustedLuaHostCallError> {
    let Value::String(value) = value else {
        return Err(standard_argument_error(format!(
            "Standard owner 必须是字符串，实际为 {}",
            value.type_name()
        )));
    };
    let value = lua_string_to_text(&value, "Standard owner").map_err(binding_error)?;
    RpgMakerStandardAssetOwner::from_storage_name(&value).ok_or_else(|| {
        standard_argument_error(format!(
            "Standard owner 无效：{value}，只接受 builtin、rules 或 lua"
        ))
    })
}

fn parse_standard_role(value: Value) -> Result<TextUnitRole, TrustedLuaHostCallError> {
    let Value::Table(role) = value else {
        return Err(standard_argument_error(format!(
            "Standard role 必须是 table，实际为 {}",
            value.type_name()
        )));
    };
    let kind = role
        .raw_get::<Value>("kind")
        .map_err(binding_error)
        .and_then(|kind| match kind {
            Value::String(kind) => {
                lua_string_to_text(&kind, "Standard role.kind").map_err(binding_error)
            }
            kind => Err(standard_argument_error(format!(
                "Standard role.kind 必须是字符串，实际为 {}",
                kind.type_name()
            ))),
        })?;
    match kind.as_str() {
        "scalar" => {
            ensure_exact_string_keys(&role, &["kind", "field"]).map_err(binding_error)?;
            let field = role
                .raw_get::<Value>("field")
                .map_err(binding_error)
                .and_then(|field| match field {
                    Value::String(field) => {
                        lua_string_to_text(&field, "Standard role.field").map_err(binding_error)
                    }
                    field => Err(standard_argument_error(format!(
                        "Standard role.field 必须是字符串，实际为 {}",
                        field.type_name()
                    ))),
                })?;
            ScalarFieldKey::new(field)
                .map(TextUnitRole::Scalar)
                .map_err(|error| {
                    TrustedLuaHostCallError::new(
                        "standard",
                        "invalid_role",
                        error.to_string(),
                        None,
                        Some(Arc::new(error)),
                    )
                })
        }
        "dialogue_speaker" | "dialogue_body" | "choices" | "scrolling_text" => {
            ensure_exact_string_keys(&role, &["kind"]).map_err(binding_error)?;
            Ok(match kind.as_str() {
                "dialogue_speaker" => TextUnitRole::DialogueSpeaker,
                "dialogue_body" => TextUnitRole::DialogueBody,
                "choices" => TextUnitRole::Choices,
                "scrolling_text" => TextUnitRole::ScrollingText,
                _ => unreachable!("外层 match 已限制 role kind"),
            })
        }
        _ => Err(standard_argument_error(format!(
            "Standard role.kind 无效：{kind}"
        ))),
    }
}

fn parse_standard_candidate_batch(
    value: Value,
    identity: &Arc<()>,
) -> Result<Vec<TrustedLuaStandardCandidate>, TrustedLuaHostCallError> {
    let Value::Table(batch) = value else {
        return Err(standard_argument_error(format!(
            "Standard accept batch 必须是无洞数组，实际为 {}",
            value.type_name()
        )));
    };
    let items = dense_values(batch, |_| {
        standard_argument_error("Standard accept batch 必须是无洞数组".to_owned())
    })?;
    items
        .into_iter()
        .enumerate()
        .map(|(index, value)| parse_standard_candidate(index, value, identity))
        .collect()
}

fn parse_standard_candidate(
    index: usize,
    value: Value,
    identity: &Arc<()>,
) -> Result<TrustedLuaStandardCandidate, TrustedLuaHostCallError> {
    let Value::Table(entry) = value else {
        return Err(standard_argument_error(format!(
            "Standard accept batch[{}] 必须是 table，实际为 {}",
            index + 1,
            value.type_name()
        )));
    };
    ensure_standard_candidate_keys(&entry)?;
    let unit = entry
        .raw_get::<AnyUserData>("unit")
        .map_err(binding_error)?;
    let unit = unit.borrow::<LuaStandardUnit>().map_err(binding_error)?;
    if !Arc::ptr_eq(identity, &unit.identity) {
        return Err(TrustedLuaHostCallError::new(
            "standard",
            "foreign_unit",
            format!("Standard accept batch[{}].unit 不属于当前会话", index + 1),
            None,
            None,
        ));
    }
    let candidate = entry.raw_get::<Value>("candidate").map_err(binding_error)?;
    let candidate = parse_standard_candidate_content(candidate, &unit.unit)?;
    let replace_current = match entry
        .raw_get::<Value>("replace_current")
        .map_err(binding_error)?
    {
        Value::Nil => false,
        Value::Boolean(value) => value,
        value => {
            return Err(standard_argument_error(format!(
                "Standard accept batch[{}].replace_current 必须是 boolean 或 nil，实际为 {}",
                index + 1,
                value.type_name()
            )));
        }
    };
    Ok(TrustedLuaStandardCandidate::new(
        unit.unit.handle(),
        candidate,
        replace_current,
    ))
}

fn ensure_standard_candidate_keys(entry: &Table) -> Result<(), TrustedLuaHostCallError> {
    let mut has_unit = false;
    let mut has_candidate = false;
    for pair in entry.clone().pairs::<Value, Value>() {
        let (key, _) = pair.map_err(binding_error)?;
        let Value::String(key) = key else {
            return Err(standard_argument_error(
                "Standard accept 候选字段名必须是字符串".to_owned(),
            ));
        };
        let key = lua_string_to_text(&key, "Standard accept 候选字段名").map_err(binding_error)?;
        match key.as_str() {
            "unit" => has_unit = true,
            "candidate" => has_candidate = true,
            "replace_current" => {}
            _ => {
                return Err(standard_argument_error(format!(
                    "Standard accept 候选包含未知字段：{key}"
                )));
            }
        }
    }
    if !has_unit {
        return Err(standard_argument_error(
            "Standard accept 候选缺少字段 unit".to_owned(),
        ));
    }
    if !has_candidate {
        return Err(standard_argument_error(
            "Standard accept 候选缺少字段 candidate".to_owned(),
        ));
    }
    Ok(())
}

fn parse_standard_candidate_content(
    value: Value,
    unit: &TrustedLuaStandardUnit,
) -> Result<TextUnitContent, TrustedLuaHostCallError> {
    match unit.content_kind() {
        super::runtime::TrustedLuaStandardContentKind::Value => {
            let Value::String(value) = value else {
                return Err(standard_argument_error(format!(
                    "Value 单元的 candidate 必须是 UTF-8 字符串，实际为 {}",
                    value.type_name()
                )));
            };
            lua_string_to_text(&value, "Standard Value candidate")
                .map(TextUnitContent::Value)
                .map_err(binding_error)
        }
        super::runtime::TrustedLuaStandardContentKind::Lines => {
            let Value::Table(lines) = value else {
                return Err(standard_argument_error(format!(
                    "Lines 单元的 candidate 必须是无洞字符串数组，实际为 {}",
                    value.type_name()
                )));
            };
            let values = dense_values(lines, |_| {
                standard_argument_error("Lines 单元的 candidate 必须是无洞字符串数组".to_owned())
            })?;
            let lines = values
                .into_iter()
                .enumerate()
                .map(|(index, value)| match value {
                    Value::String(value) => lua_string_to_text(
                        &value,
                        &format!("Standard Lines candidate[{}]", index + 1),
                    )
                    .map_err(binding_error),
                    value => Err(standard_argument_error(format!(
                        "Standard Lines candidate[{}] 必须是 UTF-8 字符串，实际为 {}",
                        index + 1,
                        value.type_name()
                    ))),
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(TextUnitContent::Lines(lines))
        }
    }
}

fn standard_acceptances_to_lua(
    lua: &Lua,
    results: Vec<TrustedLuaStandardAcceptance>,
    markers: &Table,
) -> mlua::Result<Value> {
    let output = lua.create_table()?;
    for (index, result) in results.into_iter().enumerate() {
        let entry = lua.create_table()?;
        match result {
            TrustedLuaStandardAcceptance::Accepted {
                translation,
                changed_locations,
            } => {
                entry.set("accepted", true)?;
                entry.set(
                    "translation",
                    standard_content_to_lua(lua, &translation, markers)?,
                )?;
                entry.set(
                    "changed_locations",
                    usize_to_lua_integer(changed_locations)?,
                )?;
            }
            TrustedLuaStandardAcceptance::Rejected { reason, details } => {
                entry.set("accepted", false)?;
                entry.set("reason", reason)?;
                for detail in details {
                    if matches!(
                        detail.name(),
                        "accepted" | "reason" | "translation" | "changed_locations"
                    ) {
                        return Err(mlua::Error::runtime(
                            "Standard 核心返回了保留名称的拒绝详情字段",
                        ));
                    }
                    let value = match detail.value() {
                        TrustedLuaStandardRejectionValue::String(value) => {
                            Value::String(lua.create_string(value)?)
                        }
                        TrustedLuaStandardRejectionValue::Integer(value) => {
                            Value::Integer(usize_to_lua_integer(*value)?)
                        }
                        TrustedLuaStandardRejectionValue::Boolean(value) => Value::Boolean(*value),
                    };
                    entry.set(detail.name(), value)?;
                }
            }
        }
        mark_json_table(markers, &entry, JsonContainerKind::Object)?;
        output.raw_set(index + 1, entry)?;
    }
    mark_json_table(markers, &output, JsonContainerKind::Array)?;
    Ok(Value::Table(output))
}

fn standard_content_to_lua(
    lua: &Lua,
    content: &TextUnitContent,
    markers: &Table,
) -> mlua::Result<Value> {
    match content {
        TextUnitContent::Value(value) => Ok(Value::String(lua.create_string(value)?)),
        TextUnitContent::Lines(lines) => {
            let output = lua.create_table()?;
            for (index, line) in lines.iter().enumerate() {
                output.raw_set(index + 1, line.as_str())?;
            }
            mark_json_table(markers, &output, JsonContainerKind::Array)?;
            Ok(Value::Table(output))
        }
    }
}

fn standard_role_to_lua(lua: &Lua, role: &TextUnitRole, markers: &Table) -> mlua::Result<Table> {
    let output = lua.create_table()?;
    output.set("kind", standard_role_name(role))?;
    if let TextUnitRole::Scalar(field) = role {
        output.set("field", field.as_str())?;
    }
    mark_json_table(markers, &output, JsonContainerKind::Object)?;
    Ok(output)
}

fn standard_role_name(role: &TextUnitRole) -> &'static str {
    match role {
        TextUnitRole::Scalar(_) => "scalar",
        TextUnitRole::DialogueSpeaker => "dialogue_speaker",
        TextUnitRole::DialogueBody => "dialogue_body",
        TextUnitRole::Choices => "choices",
        TextUnitRole::ScrollingText => "scrolling_text",
    }
}

fn standard_group_kind_name(kind: TextGroupKind) -> &'static str {
    kind.storage_name()
}

fn usize_to_lua_integer(value: usize) -> mlua::Result<i64> {
    i64::try_from(value).map_err(|_| mlua::Error::runtime("Standard 数量无法表示为 Lua integer"))
}

fn standard_argument_error(message: String) -> TrustedLuaHostCallError {
    TrustedLuaHostCallError::new("standard", "invalid_argument", message, None, None)
}

fn build_translation_table(
    lua: &Lua,
    calls: Arc<dyn TrustedLuaTranslateHostCalls>,
) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("system_prompt", calls.system_prompt())?;
    let language_pair = lua.create_table()?;
    language_pair.set("source", calls.source_language())?;
    language_pair.set("target", calls.target_language())?;
    table.set("language_pair", language_pair)?;

    let native = lua.create_function(
        move |lua, (kind, original, semantic_context): (Value, Value, Value)| {
            let result = parse_translation_prepare(kind, original, semantic_context)
                .and_then(|(kind, original, semantic_context)| {
                    calls.prepare_translation(kind, original, semantic_context)
                })
                .map_err(|error| error.with_operation("translation.prepare"));
            host_result_to_lua(lua, result, |lua, prepared| {
                prepared_translation_to_lua(lua, prepared)
            })
        },
    )?;
    table.set("prepare", checked_host_function(lua, native)?)?;
    Ok(table)
}

fn parse_translation_prepare(
    kind: Value,
    original: Value,
    semantic_context: Value,
) -> Result<(TextGroupKind, String, String), TrustedLuaHostCallError> {
    let Value::String(kind) = kind else {
        return Err(binding_error(mlua::Error::runtime(format!(
            "translation.prepare kind 必须是字符串，实际为 {}",
            kind.type_name()
        ))));
    };
    let kind_name = lua_string_to_text(&kind, "translation.prepare kind").map_err(binding_error)?;
    let kind = match kind_name.as_str() {
        "dialogue" => TextGroupKind::EventDialogue,
        "choices" => TextGroupKind::EventChoices,
        "scrolling_text" => TextGroupKind::EventScrollingText,
        _ => TextGroupKind::from_storage_name(&kind_name)
            .filter(|kind| {
                !matches!(
                    kind,
                    TextGroupKind::EventDialogue
                        | TextGroupKind::EventChoices
                        | TextGroupKind::EventScrollingText
                )
            })
            .ok_or_else(|| {
                binding_error(mlua::Error::runtime(format!(
                    "translation.prepare kind 无效：{kind_name}"
                )))
            })?,
    };
    let Value::String(original) = original else {
        return Err(binding_error(mlua::Error::runtime(format!(
            "translation.prepare original 必须是字符串，实际为 {}",
            original.type_name()
        ))));
    };
    let original =
        lua_string_to_text(&original, "translation.prepare original").map_err(binding_error)?;
    let Value::String(semantic_context) = semantic_context else {
        return Err(binding_error(mlua::Error::runtime(format!(
            "translation.prepare semantic_context 必须是字符串，实际为 {}",
            semantic_context.type_name()
        ))));
    };
    let semantic_context =
        lua_string_to_text(&semantic_context, "translation.prepare semantic_context")
            .map_err(binding_error)?;
    Ok((kind, original, semantic_context))
}

fn prepared_translation_to_lua(
    lua: &Lua,
    prepared: Arc<dyn TrustedLuaPreparedTranslation>,
) -> mlua::Result<Value> {
    let table = lua.create_table()?;
    table.set("status", prepared.status().as_str())?;
    table.set("model_text", prepared.model_text())?;
    let terms = lua.create_table_with_capacity(prepared.terms().len(), 0)?;
    for (index, term) in prepared.terms().iter().enumerate() {
        let value = lua.create_table()?;
        value.set("term", term.term())?;
        value.set("translation", term.translation())?;
        terms.raw_set(index + 1, value)?;
    }
    table.set("terms", terms)?;

    let current_prepared = Arc::clone(&prepared);
    let current = lua.create_function(
        move |lua, (_self, translation, state): (Value, Value, Value)| {
            let result =
                parse_prepared_translation_text(translation, "PreparedText:is_current translation")
                    .and_then(|translation| {
                        parse_translation_state(state).map(|state| (translation, state))
                    })
                    .and_then(|(translation, state)| {
                        current_prepared.is_current(translation, state)
                    })
                    .map_err(|error| error.with_operation("translation.is_current"));
            host_result_to_lua(lua, result, |_, current| Ok(Value::Boolean(current)))
        },
    )?;
    table.set("is_current", checked_host_function(lua, current)?)?;

    let native_prepared = Arc::clone(&prepared);
    let native = lua.create_function(move |lua, (_self, candidate): (Value, Value)| {
        let result = parse_translation_candidate(candidate)
            .and_then(|candidate| native_prepared.accept(candidate))
            .map_err(|error| error.with_operation("translation.accept"));
        host_result_to_lua(lua, result, prepared_acceptance_to_lua)
    })?;
    table.set("accept", checked_host_function(lua, native)?)?;
    Ok(Value::Table(table))
}

fn parse_translation_candidate(candidate: Value) -> Result<String, TrustedLuaHostCallError> {
    parse_prepared_translation_text(candidate, "PreparedText:accept candidate")
}

fn parse_prepared_translation_text(
    value: Value,
    role: &str,
) -> Result<String, TrustedLuaHostCallError> {
    let Value::String(value) = value else {
        return Err(binding_error(mlua::Error::runtime(format!(
            "{role} 必须是字符串，实际为 {}",
            value.type_name()
        ))));
    };
    let value = lua_string_to_text(&value, role).map_err(binding_error)?;
    Ok(value)
}

fn parse_translation_state(value: Value) -> Result<Sha256Fingerprint, TrustedLuaHostCallError> {
    let Value::String(value) = value else {
        return Err(invalid_translation_state(format!(
            "PreparedText:is_current state 必须是 64 位小写十六进制字符串，实际为 {}",
            value.type_name()
        )));
    };
    let bytes = value.as_bytes();
    if bytes.len() != SHA256_FINGERPRINT_BYTES * 2
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(invalid_translation_state(
            "PreparedText:is_current state 必须是 64 位小写十六进制 SHA-256 文本",
        ));
    }
    let mut decoded = [0_u8; SHA256_FINGERPRINT_BYTES];
    for (index, pair) in bytes.chunks_exact(2).enumerate() {
        decoded[index] = (hex_nibble(pair[0]) << 4) | hex_nibble(pair[1]);
    }
    Ok(Sha256Fingerprint::from_bytes(decoded))
}

fn hex_nibble(value: u8) -> u8 {
    match value {
        b'0'..=b'9' => value - b'0',
        b'a'..=b'f' => value - b'a' + 10,
        _ => unreachable!("state 格式已验证为小写十六进制"),
    }
}

fn invalid_translation_state(message: impl Into<String>) -> TrustedLuaHostCallError {
    TrustedLuaHostCallError::new("translation", "invalid_state", message, None, None)
}

fn translation_state_text(state: Sha256Fingerprint) -> String {
    state.hex()
}

fn prepared_acceptance_to_lua(
    lua: &Lua,
    acceptance: TrustedLuaPreparedTranslationAcceptance,
) -> mlua::Result<Value> {
    let table = lua.create_table()?;
    match acceptance {
        TrustedLuaPreparedTranslationAcceptance::Accepted { translation, state } => {
            table.set("accepted", true)?;
            table.set("translation", translation)?;
            table.set("state", translation_state_text(state))?;
        }
        TrustedLuaPreparedTranslationAcceptance::Rejected { reason } => {
            table.set("accepted", false)?;
            table.set("reason", reason)?;
        }
    }
    Ok(Value::Table(table))
}

fn install_write_back_context(
    lua: &Lua,
    context: &Table,
    calls: Arc<dyn TrustedLuaWriteBackHostCalls>,
    tokio: Handle,
    cancellation: RuntimeCancellation,
    markers: &Table,
) -> mlua::Result<()> {
    context.set(
        "output",
        build_output_table(
            lua,
            Arc::clone(&calls),
            tokio.clone(),
            cancellation.clone(),
            markers,
        )?,
    )?;

    let write_back = lua.create_table()?;
    let layout_calls = Arc::clone(&calls);
    let layout = lua.create_function(move |lua, (region, pairs): (Value, Value)| {
        let result = parse_write_back_layout(region, pairs)
            .and_then(|(region, pairs)| layout_calls.layout(region, pairs))
            .map_err(|error| error.with_operation("write_back.layout"));
        host_result_to_lua(lua, result, write_back_layout_result_to_lua)
    })?;
    write_back.set("layout", checked_host_function(lua, layout)?)?;
    context.set("write_back", write_back)?;
    install_write_back_translations_context(lua, context, calls, tokio, cancellation, markers)
}

fn build_output_table(
    lua: &Lua,
    calls: Arc<dyn TrustedLuaWriteBackHostCalls>,
    tokio: Handle,
    cancellation: RuntimeCancellation,
    markers: &Table,
) -> mlua::Result<Table> {
    let native = lua.create_table()?;

    let read_calls = Arc::clone(&calls);
    let read_tokio = tokio.clone();
    let read_cancellation = cancellation.clone();
    native.set(
        "read",
        lua.create_function(move |lua, path: Value| {
            let result = parse_output_path(path)
                .and_then(|path| {
                    wait_for_output_terminal(
                        &read_tokio,
                        &read_cancellation,
                        read_calls.read_output(path),
                    )
                })
                .map_err(|error| error.with_operation("output.read"));
            host_result_to_lua(lua, result, |lua, bytes| {
                lua.create_string(bytes).map(Value::String)
            })
        })?,
    )?;

    let text_calls = Arc::clone(&calls);
    let text_tokio = tokio.clone();
    let text_cancellation = cancellation.clone();
    native.set(
        "read_text",
        lua.create_function(move |lua, path: Value| {
            let result = parse_output_path(path)
                .and_then(|path| {
                    wait_for_output_terminal(
                        &text_tokio,
                        &text_cancellation,
                        text_calls.read_output(path),
                    )
                })
                .map_err(|error| error.with_operation("output.read_text"));
            host_result_to_lua(lua, result, |lua, bytes| {
                let text = std::str::from_utf8(&bytes)
                    .map_err(|_| mlua::Error::runtime("写回候选文件不是有效 UTF-8"))?;
                lua.create_string(text).map(Value::String)
            })
        })?,
    )?;

    let json_calls = Arc::clone(&calls);
    let json_tokio = tokio.clone();
    let json_cancellation = cancellation.clone();
    let read_json_markers = markers.clone();
    native.set(
        "read_json",
        lua.create_function(move |lua, path: Value| {
            let result = parse_output_path(path)
                .and_then(|path| {
                    wait_for_output_terminal(
                        &json_tokio,
                        &json_cancellation,
                        json_calls.read_output(path),
                    )
                })
                .map_err(|error| error.with_operation("output.read_json"));
            host_result_to_lua(lua, result, |lua, bytes| {
                let text = std::str::from_utf8(&bytes)
                    .map_err(|_| mlua::Error::runtime("写回候选 JSON 文件不是有效 UTF-8"))?;
                let value = decode_json(text).map_err(|error| {
                    mlua::Error::runtime(format!("写回候选 JSON 无效：{error}"))
                })?;
                lossless_json_to_lua(lua, value, &read_json_markers)
            })
        })?,
    )?;

    let list_calls = Arc::clone(&calls);
    let list_tokio = tokio.clone();
    let list_cancellation = cancellation.clone();
    let list_markers = markers.clone();
    native.set(
        "list",
        lua.create_function(move |lua, path: Value| {
            let result = parse_output_path(path)
                .and_then(|path| {
                    wait_for_output_terminal(
                        &list_tokio,
                        &list_cancellation,
                        list_calls.list_output(path),
                    )
                })
                .map_err(|error| error.with_operation("output.list"));
            host_result_to_lua(lua, result, |lua, entries| {
                let entries = entries
                    .into_iter()
                    .map(|entry| {
                        LosslessJsonValue::Object(vec![
                            (
                                "kind".to_owned(),
                                LosslessJsonValue::String(entry.kind().as_str().to_owned()),
                            ),
                            (
                                "name".to_owned(),
                                LosslessJsonValue::String(entry.name().to_owned()),
                            ),
                        ])
                    })
                    .collect();
                lossless_json_to_lua(lua, LosslessJsonValue::Array(entries), &list_markers)
            })
        })?,
    )?;

    let create_calls = Arc::clone(&calls);
    let create_tokio = tokio.clone();
    let create_cancellation = cancellation.clone();
    native.set(
        "create_directory",
        lua.create_function(move |lua, path: Value| {
            let result = parse_output_path(path)
                .and_then(|path| {
                    wait_for_output_terminal(
                        &create_tokio,
                        &create_cancellation,
                        create_calls.create_output_directory(path),
                    )
                })
                .map_err(|error| error.with_operation("output.create_directory"));
            host_result_to_lua(lua, result, |_, ()| Ok(Value::Nil))
        })?,
    )?;

    let write_calls = Arc::clone(&calls);
    let write_tokio = tokio.clone();
    let write_cancellation = cancellation.clone();
    native.set(
        "write",
        lua.create_function(move |lua, (path, bytes): (Value, Value)| {
            let result = parse_output_path(path)
                .and_then(|path| {
                    let Value::String(bytes) = bytes else {
                        return Err(binding_error(mlua::Error::runtime(format!(
                            "ctx.output.write bytes 必须是字符串，实际为 {}",
                            bytes.type_name()
                        ))));
                    };
                    wait_for_output_terminal(
                        &write_tokio,
                        &write_cancellation,
                        write_calls.write_output(path, bytes.as_bytes().to_vec()),
                    )
                })
                .map_err(|error| error.with_operation("output.write"));
            host_result_to_lua(lua, result, |_, ()| Ok(Value::Nil))
        })?,
    )?;

    let write_text_calls = Arc::clone(&calls);
    let write_text_tokio = tokio.clone();
    let write_text_cancellation = cancellation.clone();
    native.set(
        "write_text",
        lua.create_function(move |lua, (path, text): (Value, Value)| {
            let result = parse_output_path(path)
                .and_then(|path| {
                    let Value::String(text) = text else {
                        return Err(binding_error(mlua::Error::runtime(format!(
                            "ctx.output.write_text text 必须是 UTF-8 字符串，实际为 {}",
                            text.type_name()
                        ))));
                    };
                    let text = lua_string_to_text(&text, "ctx.output.write_text text")
                        .map_err(binding_error)?;
                    wait_for_output_terminal(
                        &write_text_tokio,
                        &write_text_cancellation,
                        write_text_calls.write_output(path, text.into_bytes()),
                    )
                })
                .map_err(|error| error.with_operation("output.write_text"));
            host_result_to_lua(lua, result, |_, ()| Ok(Value::Nil))
        })?,
    )?;

    let write_json_calls = Arc::clone(&calls);
    let write_json_tokio = tokio.clone();
    let write_json_cancellation = cancellation.clone();
    let write_json_markers = markers.clone();
    native.set(
        "write_json",
        lua.create_function(move |lua, (path, value): (Value, Value)| {
            let result = parse_output_path(path)
                .and_then(|path| {
                    let encoded = JsonEncoder::new(&write_json_markers)
                        .encode(value)
                        .map_err(binding_error)?;
                    wait_for_output_terminal(
                        &write_json_tokio,
                        &write_json_cancellation,
                        write_json_calls.write_output(path, encoded.into_bytes()),
                    )
                })
                .map_err(|error| error.with_operation("output.write_json"));
            host_result_to_lua(lua, result, |_, ()| Ok(Value::Nil))
        })?,
    )?;

    let remove_calls = calls;
    native.set(
        "remove",
        lua.create_function(move |lua, path: Value| {
            let result = parse_output_path(path)
                .and_then(|path| {
                    wait_for_output_terminal(
                        &tokio,
                        &cancellation,
                        remove_calls.remove_output(path),
                    )
                })
                .map_err(|error| error.with_operation("output.remove"));
            host_result_to_lua(lua, result, |_, ()| Ok(Value::Nil))
        })?,
    )?;

    checked_function_table(
        lua,
        native,
        &[
            "read",
            "read_text",
            "read_json",
            "list",
            "create_directory",
            "write",
            "write_text",
            "write_json",
            "remove",
        ],
    )
}

fn parse_output_path(value: Value) -> Result<ScopedDirectoryPath, TrustedLuaHostCallError> {
    let Value::String(value) = value else {
        return Err(binding_error(mlua::Error::runtime(format!(
            "候选路径必须是 UTF-8 字符串，实际为 {}",
            value.type_name()
        ))));
    };
    let value = lua_string_to_text(&value, "候选路径").map_err(binding_error)?;
    ScopedDirectoryPath::new(PathBuf::from(value)).map_err(|error| {
        TrustedLuaHostCallError::new(
            "binding",
            "invalid_output_path",
            error.to_string(),
            None,
            Some(Arc::new(error)),
        )
    })
}

fn parse_write_back_layout(
    region: Value,
    pairs: Value,
) -> Result<
    (
        TrustedLuaWriteBackLayoutRegion,
        Vec<TrustedLuaWriteBackLayoutPair>,
    ),
    TrustedLuaHostCallError,
> {
    let Value::String(region) = region else {
        return Err(binding_error(mlua::Error::runtime(format!(
            "ctx.write_back.layout region 必须是字符串，实际为 {}",
            region.type_name()
        ))));
    };
    let region_name =
        lua_string_to_text(&region, "ctx.write_back.layout region").map_err(binding_error)?;
    let region = match region_name.as_str() {
        "dialogue_body" => TrustedLuaWriteBackLayoutRegion::DialogueBody,
        "scrolling_text" => TrustedLuaWriteBackLayoutRegion::ScrollingText,
        "help_description" => TrustedLuaWriteBackLayoutRegion::HelpDescription,
        _ => {
            return Err(binding_error(mlua::Error::runtime(format!(
                "ctx.write_back.layout region 无效：{region_name}"
            ))));
        }
    };
    let Value::Table(pairs) = pairs else {
        return Err(binding_error(mlua::Error::runtime(format!(
            "ctx.write_back.layout pairs 必须是无洞数组，实际为 {}",
            pairs.type_name()
        ))));
    };
    let pairs = dense_values(pairs, binding_error)?
        .into_iter()
        .enumerate()
        .map(|(index, pair)| parse_write_back_layout_pair(index, pair))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((region, pairs))
}

fn parse_write_back_layout_pair(
    index: usize,
    pair: Value,
) -> Result<TrustedLuaWriteBackLayoutPair, TrustedLuaHostCallError> {
    let Value::Table(pair) = pair else {
        return Err(binding_error(mlua::Error::runtime(format!(
            "ctx.write_back.layout pairs[{}] 必须是 table，实际为 {}",
            index + 1,
            pair.type_name()
        ))));
    };
    for field in pair.clone().pairs::<Value, Value>() {
        let (field, _) = field.map_err(binding_error)?;
        let Value::String(field) = field else {
            return Err(binding_error(mlua::Error::runtime(format!(
                "ctx.write_back.layout pairs[{}] 只允许字符串字段",
                index + 1
            ))));
        };
        let field =
            lua_string_to_text(&field, "ctx.write_back.layout pair 字段").map_err(binding_error)?;
        if field != "original" && field != "translation" {
            return Err(binding_error(mlua::Error::runtime(format!(
                "ctx.write_back.layout pairs[{}] 包含未知字段：{field}",
                index + 1
            ))));
        }
    }

    let original = match pair.raw_get::<Value>("original").map_err(binding_error)? {
        Value::String(value) => {
            lua_string_to_text(&value, "layout original").map_err(binding_error)?
        }
        value => {
            return Err(binding_error(mlua::Error::runtime(format!(
                "ctx.write_back.layout pairs[{}].original 必须是 UTF-8 字符串，实际为 {}",
                index + 1,
                value.type_name()
            ))));
        }
    };
    let translation = match pair
        .raw_get::<Value>("translation")
        .map_err(binding_error)?
    {
        Value::Nil => None,
        Value::String(value) => {
            Some(lua_string_to_text(&value, "layout translation").map_err(binding_error)?)
        }
        value => {
            return Err(binding_error(mlua::Error::runtime(format!(
                "ctx.write_back.layout pairs[{}].translation 必须是 UTF-8 字符串或 nil，实际为 {}",
                index + 1,
                value.type_name()
            ))));
        }
    };
    Ok(TrustedLuaWriteBackLayoutPair::new(original, translation))
}

fn write_back_layout_result_to_lua(
    lua: &Lua,
    result: TrustedLuaWriteBackLayoutResult,
) -> mlua::Result<Value> {
    let table = lua.create_table()?;
    table.set("status", result.status().as_str())?;
    let texts = lua.create_table_with_capacity(result.texts().len(), 0)?;
    for (index, text) in result.texts().iter().enumerate() {
        texts.raw_set(index + 1, text.as_str())?;
    }
    table.set("texts", texts)?;
    table.set(
        "inserted_line_breaks",
        i64::try_from(result.inserted_line_breaks())
            .map_err(|_| mlua::Error::runtime("布局新增换行数超出 Lua integer"))?,
    )?;
    table.set(
        "inserted_fullwidth_indents",
        i64::try_from(result.inserted_fullwidth_indents())
            .map_err(|_| mlua::Error::runtime("布局新增全角缩进数超出 Lua integer"))?,
    )?;
    Ok(Value::Table(table))
}

fn phase_name(phase: LuaPhase) -> &'static str {
    match phase {
        LuaPhase::Extract => "extract",
        LuaPhase::Translate => "translate",
        LuaPhase::WriteBack => "write_back",
        LuaPhase::Project => "lua",
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum JsonContainerKind {
    Array,
    Object,
}

impl JsonContainerKind {
    const fn marker(self) -> i64 {
        match self {
            Self::Array => 1,
            Self::Object => 2,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Array => "array",
            Self::Object => "object",
        }
    }
}

fn build_json_table(lua: &Lua, markers: &Table) -> mlua::Result<Table> {
    let native = lua.create_table()?;

    for (name, kind) in [
        ("array", JsonContainerKind::Array),
        ("object", JsonContainerKind::Object),
    ] {
        let markers = markers.clone();
        native.set(
            name,
            lua.create_function(move |lua, value: Value| {
                let result = (|| {
                    let table = match value {
                        Value::Nil => lua.create_table()?,
                        Value::Table(table) => table,
                        value => {
                            return Err(mlua::Error::runtime(format!(
                                "ctx.json.{} 只接受 table 或 nil，实际为 {}",
                                kind.name(),
                                value.type_name()
                            )));
                        }
                    };
                    mark_json_table(&markers, &table, kind)?;
                    Ok(Value::Table(table))
                })();
                json_result_to_lua(lua, kind.name(), result)
            })?,
        )?;
    }

    native.set(
        "number",
        lua.create_function(move |lua, value: Value| {
            let result = (|| {
                let Value::String(value) = value else {
                    return Err(mlua::Error::runtime(
                        "ctx.json.number 的参数必须是 UTF-8 字符串",
                    ));
                };
                let value = value.to_str().map_err(|_| {
                    mlua::Error::runtime("ctx.json.number 的参数必须是 UTF-8 字符串")
                })?;
                validate_number(value.as_ref())
                    .map_err(|error| mlua::Error::runtime(format!("ctx.json.number：{error}")))?;
                json_number_to_lua(lua, value.to_owned())
            })();
            json_result_to_lua(lua, "number", result)
        })?,
    )?;

    let decode_markers = markers.clone();
    native.set(
        "decode",
        lua.create_function(move |lua, source: Value| {
            let result = (|| {
                let Value::String(source) = source else {
                    return Err(mlua::Error::runtime(
                        "ctx.json.decode 的参数必须是 UTF-8 字符串",
                    ));
                };
                let source = source.to_str().map_err(|_| {
                    mlua::Error::runtime("ctx.json.decode 的参数必须是 UTF-8 字符串")
                })?;
                let value = decode_json(source.as_ref())
                    .map_err(|error| mlua::Error::runtime(format!("ctx.json.decode：{error}")))?;
                lossless_json_to_lua(lua, value, &decode_markers)
            })();
            json_result_to_lua(lua, "decode", result)
        })?,
    )?;

    let encode_markers = markers.clone();
    native.set(
        "encode",
        lua.create_function(move |lua, value: Value| {
            let result = JsonEncoder::new(&encode_markers)
                .encode(value)
                .and_then(|encoded| lua.create_string(encoded).map(Value::String));
            json_result_to_lua(lua, "encode", result)
        })?,
    )?;

    let kind_markers = markers.clone();
    native.set(
        "kind",
        lua.create_function(move |lua, value: Value| {
            let result = json_kind(&kind_markers, &value).and_then(|kind| match kind {
                Some(kind) => lua.create_string(kind).map(Value::String),
                None => Ok(Value::Nil),
            });
            json_result_to_lua(lua, "kind", result)
        })?,
    )?;
    native.set(
        "number_text",
        lua.create_function(move |lua, value: Value| {
            let result = json_number_text(&value)
                .and_then(|text| lua.create_string(text).map(Value::String));
            json_result_to_lua(lua, "number_text", result)
        })?,
    )?;
    let json = checked_function_table(
        lua,
        native,
        &[
            "array",
            "object",
            "number",
            "decode",
            "encode",
            "kind",
            "number_text",
        ],
    )?;
    json.set("NULL", lua.create_userdata(LuaJsonNull)?)?;
    Ok(json)
}

fn json_result_to_lua(
    lua: &Lua,
    operation: &'static str,
    result: mlua::Result<Value>,
) -> mlua::Result<MultiValue> {
    host_result_to_lua(
        lua,
        result.map_err(json_host_error).map_err(|error| {
            error.with_operation(match operation {
                "array" => "json.array",
                "object" => "json.object",
                "number" => "json.number",
                "decode" => "json.decode",
                "encode" => "json.encode",
                "kind" => "json.kind",
                "number_text" => "json.number_text",
                _ => "json.unknown",
            })
        }),
        |_, value| Ok(value),
    )
}

fn json_host_error(_error: mlua::Error) -> TrustedLuaHostCallError {
    TrustedLuaHostCallError::new(
        "json",
        "invalid_value",
        "Lua Host JSON 值不符合协议",
        None,
        None,
    )
}

fn mark_json_table(markers: &Table, table: &Table, kind: JsonContainerKind) -> mlua::Result<()> {
    match json_table_kind(markers, table)? {
        Some(current) if current == kind => Ok(()),
        Some(current) => Err(mlua::Error::runtime(format!(
            "JSON table 已标记为 {}，不能再标记为 {}",
            current.name(),
            kind.name()
        ))),
        None => markers.raw_set(table.clone(), kind.marker()),
    }
}

fn json_table_kind(markers: &Table, table: &Table) -> mlua::Result<Option<JsonContainerKind>> {
    match markers.raw_get::<Value>(table.clone())? {
        Value::Nil => Ok(None),
        Value::Integer(1) => Ok(Some(JsonContainerKind::Array)),
        Value::Integer(2) => Ok(Some(JsonContainerKind::Object)),
        _ => Err(mlua::Error::runtime("JSON table 标记已损坏")),
    }
}

fn lossless_json_to_lua(
    lua: &Lua,
    value: LosslessJsonValue,
    markers: &Table,
) -> mlua::Result<Value> {
    enum Key {
        Index(usize),
        String(String),
    }

    let root = lua.create_table_with_capacity(1, 0)?;
    let mut work = vec![(root.clone(), Key::Index(1), value)];
    while let Some((destination, key, mut value)) = work.pop() {
        let converted = match &mut value {
            LosslessJsonValue::Null => Value::UserData(lua.create_userdata(LuaJsonNull)?),
            LosslessJsonValue::Boolean(value) => Value::Boolean(*value),
            LosslessJsonValue::String(value) => {
                Value::String(lua.create_string(std::mem::take(value))?)
            }
            LosslessJsonValue::Number(value) => json_number_to_lua(lua, std::mem::take(value))?,
            LosslessJsonValue::Array(values) => {
                let values = std::mem::take(values);
                let table = lua.create_table_with_capacity(values.len(), 0)?;
                mark_json_table(markers, &table, JsonContainerKind::Array)?;
                for (index, value) in values.into_iter().enumerate().rev() {
                    work.push((table.clone(), Key::Index(index + 1), value));
                }
                Value::Table(table)
            }
            LosslessJsonValue::Object(entries) => {
                let entries = std::mem::take(entries);
                let table = lua.create_table_with_capacity(0, entries.len())?;
                mark_json_table(markers, &table, JsonContainerKind::Object)?;
                for (key, value) in entries.into_iter().rev() {
                    work.push((table.clone(), Key::String(key), value));
                }
                Value::Table(table)
            }
        };
        match key {
            Key::Index(index) => destination.raw_set(index, converted)?,
            Key::String(key) => destination.raw_set(key, converted)?,
        }
    }
    root.raw_get(1)
}

fn json_number_to_lua(lua: &Lua, value: String) -> mlua::Result<Value> {
    if let Ok(integer) = value.parse::<i64>()
        && integer.to_string() == value
    {
        Ok(Value::Integer(integer))
    } else {
        Ok(Value::UserData(lua.create_userdata(LuaJsonNumber(value))?))
    }
}

fn json_kind(markers: &Table, value: &Value) -> mlua::Result<Option<&'static str>> {
    match value {
        Value::Boolean(_) => Ok(Some("boolean")),
        Value::Integer(_) => Ok(Some("number")),
        Value::Number(value) if value.is_finite() => Ok(Some("number")),
        Value::String(value) if value.to_str().is_ok() => Ok(Some("string")),
        Value::Table(table) => Ok(json_table_kind(markers, table)?.map(JsonContainerKind::name)),
        Value::UserData(value) if value.is::<LuaJsonNull>() => Ok(Some("null")),
        Value::UserData(value) if value.is::<LuaJsonNumber>() => Ok(Some("number")),
        _ => Ok(None),
    }
}

fn json_number_text(value: &Value) -> mlua::Result<String> {
    match value {
        Value::Integer(value) => Ok(value.to_string()),
        Value::Number(value) => serde_json::Number::from_f64(*value)
            .map(|value| value.to_string())
            .ok_or_else(|| mlua::Error::runtime("JSON number 不能是 NaN 或 Infinity")),
        Value::UserData(value) if value.is::<LuaJsonNumber>() => {
            Ok(value.borrow::<LuaJsonNumber>()?.0.clone())
        }
        value => Err(mlua::Error::runtime(format!(
            "ctx.json.number_text 只接受 JSON number，实际为 {}",
            value.type_name()
        ))),
    }
}

struct JsonEncoder<'a> {
    markers: &'a Table,
    output: String,
    active_tables: HashSet<usize>,
}

impl<'a> JsonEncoder<'a> {
    fn new(markers: &'a Table) -> Self {
        Self {
            markers,
            output: String::new(),
            active_tables: HashSet::new(),
        }
    }

    fn encode(mut self, value: Value) -> mlua::Result<String> {
        let mut work = vec![JsonEncodeAction::Value(value)];
        while let Some(action) = work.pop() {
            match action {
                JsonEncodeAction::StaticText(text) => self.output.push_str(text),
                JsonEncodeAction::OwnedText(text) => self.output.push_str(&text),
                JsonEncodeAction::FinishTable(identity) => {
                    self.active_tables.remove(&identity);
                }
                JsonEncodeAction::Value(Value::Boolean(value)) => {
                    self.output.push_str(if value { "true" } else { "false" });
                }
                JsonEncodeAction::Value(Value::Integer(value)) => {
                    self.output.push_str(&value.to_string());
                }
                JsonEncodeAction::Value(Value::Number(value)) => {
                    let value = serde_json::Number::from_f64(value).ok_or_else(|| {
                        mlua::Error::runtime("JSON number 不能是 NaN 或 Infinity")
                    })?;
                    self.output.push_str(&value.to_string());
                }
                JsonEncodeAction::Value(Value::String(value)) => {
                    let value = value
                        .to_str()
                        .map_err(|_| mlua::Error::runtime("JSON string 必须是 UTF-8"))?;
                    self.push_json_string(value.as_ref());
                }
                JsonEncodeAction::Value(Value::Table(table)) => {
                    let identity = table.to_pointer() as usize;
                    if !self.active_tables.insert(identity) {
                        return Err(mlua::Error::runtime("ctx.json.encode 不接受循环 table"));
                    }
                    let mut actions = match json_table_kind(self.markers, &table)? {
                        Some(JsonContainerKind::Array) => self.array_actions(table)?,
                        Some(JsonContainerKind::Object) => self.object_actions(table)?,
                        None => {
                            return Err(mlua::Error::runtime(
                                "ctx.json.encode 的 table 必须先由 ctx.json.array 或 ctx.json.object 显式标记",
                            ));
                        }
                    };
                    actions.push(JsonEncodeAction::FinishTable(identity));
                    work.extend(actions.into_iter().rev());
                }
                JsonEncodeAction::Value(Value::UserData(value)) if value.is::<LuaJsonNull>() => {
                    self.output.push_str("null");
                }
                JsonEncodeAction::Value(Value::UserData(value)) if value.is::<LuaJsonNumber>() => {
                    self.output
                        .push_str(value.borrow::<LuaJsonNumber>()?.0.as_str());
                }
                JsonEncodeAction::Value(Value::UserData(_)) => {
                    return Err(mlua::Error::runtime(
                        "ctx.json.encode 不接受非 JSON userdata",
                    ));
                }
                JsonEncodeAction::Value(value) => {
                    return Err(mlua::Error::runtime(format!(
                        "ctx.json.encode 不接受 {}",
                        value.type_name()
                    )));
                }
            }
        }
        Ok(self.output)
    }

    fn array_actions(&self, table: Table) -> mlua::Result<Vec<JsonEncodeAction>> {
        let mut maximum = 0_usize;
        let mut count = 0_usize;
        for pair in table.clone().pairs::<Value, Value>() {
            let (key, _value) = pair?;
            let Value::Integer(key) = key else {
                return Err(mlua::Error::runtime(
                    "JSON array 只允许从 1 开始的连续整数键",
                ));
            };
            let key = usize::try_from(key)
                .map_err(|_| mlua::Error::runtime("JSON array 只允许从 1 开始的连续整数键"))?;
            if key == 0 {
                return Err(mlua::Error::runtime(
                    "JSON array 只允许从 1 开始的连续整数键",
                ));
            }
            count += 1;
            maximum = maximum.max(key);
        }
        if count != maximum {
            return Err(mlua::Error::runtime("JSON array 不允许洞"));
        }

        let mut actions = Vec::with_capacity(maximum.saturating_mul(2).saturating_add(1));
        actions.push(JsonEncodeAction::StaticText("["));
        for index in 1..=maximum {
            if index != 1 {
                actions.push(JsonEncodeAction::StaticText(","));
            }
            actions.push(JsonEncodeAction::Value(table.raw_get::<Value>(index)?));
        }
        actions.push(JsonEncodeAction::StaticText("]"));
        Ok(actions)
    }

    fn object_actions(&self, table: Table) -> mlua::Result<Vec<JsonEncodeAction>> {
        let mut entries = Vec::new();
        for pair in table.pairs::<Value, Value>() {
            let (key, value) = pair?;
            let Value::String(key) = key else {
                return Err(mlua::Error::runtime("JSON object 只允许 UTF-8 字符串键"));
            };
            let key = key
                .to_str()
                .map_err(|_| mlua::Error::runtime("JSON object 只允许 UTF-8 字符串键"))?
                .to_owned();
            entries.push((key, value));
        }
        entries.sort_by(|left, right| left.0.cmp(&right.0));

        let mut actions = Vec::with_capacity(entries.len().saturating_mul(4).saturating_add(1));
        actions.push(JsonEncodeAction::StaticText("{"));
        for (index, (key, value)) in entries.into_iter().enumerate() {
            if index != 0 {
                actions.push(JsonEncodeAction::StaticText(","));
            }
            actions.push(JsonEncodeAction::OwnedText(
                serde_json::to_string(&key).expect("有效 UTF-8 字符串必须可以序列化为 JSON string"),
            ));
            actions.push(JsonEncodeAction::StaticText(":"));
            actions.push(JsonEncodeAction::Value(value));
        }
        actions.push(JsonEncodeAction::StaticText("}"));
        Ok(actions)
    }

    fn push_json_string(&mut self, value: &str) {
        let encoded =
            serde_json::to_string(value).expect("有效 UTF-8 字符串必须可以序列化为 JSON string");
        self.output.push_str(&encoded);
    }
}

enum JsonEncodeAction {
    Value(Value),
    StaticText(&'static str),
    OwnedText(String),
    FinishTable(usize),
}

fn build_source_table(
    lua: &Lua,
    calls: Arc<dyn TrustedLuaCommonHostCalls>,
    tokio: Handle,
    cancellation: RuntimeCancellation,
    markers: &Table,
) -> mlua::Result<Table> {
    let native = lua.create_table()?;

    let read_calls = Arc::clone(&calls);
    let read_tokio = tokio.clone();
    let read_cancellation = cancellation.clone();
    native.set(
        "read",
        lua.create_function(move |lua, path: Value| {
            let result = parse_source_path(path)
                .and_then(|path| {
                    wait_for_host(
                        &read_tokio,
                        &read_cancellation,
                        read_calls.read_source(path),
                    )
                })
                .map_err(|error| error.with_operation("source.read"));
            host_result_to_lua(lua, result, |lua, bytes| {
                lua.create_string(bytes).map(Value::String)
            })
        })?,
    )?;

    let text_calls = Arc::clone(&calls);
    let text_tokio = tokio.clone();
    let text_cancellation = cancellation.clone();
    native.set(
        "read_text",
        lua.create_function(move |lua, path: Value| {
            let result = parse_source_path(path)
                .and_then(|path| {
                    wait_for_host(
                        &text_tokio,
                        &text_cancellation,
                        text_calls.read_source(path),
                    )
                })
                .map_err(|error| error.with_operation("source.read_text"));
            host_result_to_lua(lua, result, |lua, bytes| {
                let text = std::str::from_utf8(&bytes)
                    .map_err(|_| mlua::Error::runtime("来源文件不是有效 UTF-8"))?;
                lua.create_string(text).map(Value::String)
            })
        })?,
    )?;

    let json_calls = Arc::clone(&calls);
    let json_tokio = tokio.clone();
    let json_cancellation = cancellation.clone();
    let json_markers = markers.clone();
    native.set(
        "read_json",
        lua.create_function(move |lua, path: Value| {
            let result = parse_source_path(path)
                .and_then(|path| {
                    wait_for_host(
                        &json_tokio,
                        &json_cancellation,
                        json_calls.read_source(path),
                    )
                })
                .map_err(|error| error.with_operation("source.read_json"));
            host_result_to_lua(lua, result, |lua, bytes| {
                let text = std::str::from_utf8(&bytes)
                    .map_err(|_| mlua::Error::runtime("来源 JSON 文件不是有效 UTF-8"))?;
                let value = decode_json(text)
                    .map_err(|error| mlua::Error::runtime(format!("来源 JSON 无效：{error}")))?;
                lossless_json_to_lua(lua, value, &json_markers)
            })
        })?,
    )?;

    let list_calls = calls;
    let list_markers = markers.clone();
    native.set(
        "list",
        lua.create_function(move |lua, path: Value| {
            let result = parse_source_path(path)
                .and_then(|path| wait_for_host(&tokio, &cancellation, list_calls.list_source(path)))
                .map_err(|error| error.with_operation("source.list"));
            host_result_to_lua(lua, result, |lua, entries| {
                let values = entries.into_iter().map(LosslessJsonValue::String).collect();
                lossless_json_to_lua(lua, LosslessJsonValue::Array(values), &list_markers)
            })
        })?,
    )?;

    checked_function_table(lua, native, &["read", "read_text", "read_json", "list"])
}

fn build_rpg_maker_table(
    lua: &Lua,
    calls: Arc<dyn TrustedLuaCommonHostCalls>,
    tokio: Handle,
    cancellation: RuntimeCancellation,
    markers: &Table,
) -> mlua::Result<Table> {
    let native = lua.create_table()?;
    native.set(
        "data",
        lua.create_function(move |lua, file_name: Value| {
            let result = parse_rpg_maker_string(file_name, "RPG Maker Data 文件名")
                .and_then(|file_name| data_source(&file_name).map_err(rpg_maker_host_error))
                .map_err(|error| error.with_operation("rpg_maker.data"));
            host_result_to_lua(lua, result, |lua, source| {
                lua.create_userdata(LuaRpgMakerSource(source))
                    .map(Value::UserData)
            })
        })?,
    )?;
    native.set(
        "data_file",
        lua.create_function(move |lua, file_name: Value| {
            let result = parse_rpg_maker_string(file_name, "RPG Maker DataFile 文件名")
                .and_then(|file_name| data_file_source(&file_name).map_err(rpg_maker_host_error))
                .map_err(|error| error.with_operation("rpg_maker.data_file"));
            host_result_to_lua(lua, result, |lua, source| {
                lua.create_userdata(LuaRpgMakerSource(source))
                    .map(Value::UserData)
            })
        })?,
    )?;
    native.set(
        "map",
        lua.create_function(|lua, map_id: Value| {
            let result = parse_rpg_maker_integer(map_id, "RPG Maker map ID")
                .and_then(|map_id| map_source(map_id).map_err(rpg_maker_host_error))
                .map_err(|error| error.with_operation("rpg_maker.map"));
            host_result_to_lua(lua, result, |lua, source| {
                lua.create_userdata(LuaRpgMakerSource(source))
                    .map(Value::UserData)
            })
        })?,
    )?;
    native.set(
        "plugin_parameter",
        lua.create_function(
            move |lua, (plugin_index, plugin_name, parameter_name): (Value, Value, Value)| {
                let result = parse_rpg_maker_integer(plugin_index, "插件索引")
                    .and_then(|plugin_index| {
                        parse_rpg_maker_string(plugin_name, "插件名").and_then(|plugin_name| {
                            parse_rpg_maker_string(parameter_name, "插件参数名").and_then(
                                |parameter_name| {
                                    plugin_parameter_source(
                                        plugin_index,
                                        &plugin_name,
                                        &parameter_name,
                                    )
                                    .map_err(rpg_maker_host_error)
                                },
                            )
                        })
                    })
                    .map_err(|error| error.with_operation("rpg_maker.plugin_parameter"));
                host_result_to_lua(lua, result, |lua, source| {
                    lua.create_userdata(LuaRpgMakerSource(source))
                        .map(Value::UserData)
                })
            },
        )?,
    )?;

    let open_markers = markers.clone();
    native.set(
        "open",
        lua.create_function(move |lua, source: Value| {
            let result = parse_rpg_maker_source(source)
                .and_then(|source| {
                    let path = source_path(&source);
                    wait_for_host(&tokio, &cancellation, calls.read_source(path))
                        .map(|bytes| (source, bytes))
                })
                .and_then(|(source, bytes)| {
                    OpenedRpgMakerDocument::open(source, &bytes).map_err(rpg_maker_host_error)
                })
                .map_err(|error| error.with_operation("rpg_maker.open"));
            host_result_to_lua(lua, result, |lua, document| {
                lua.create_userdata(LuaRpgMakerDocument {
                    document,
                    markers: open_markers.clone(),
                })
                .map(Value::UserData)
            })
        })?,
    )?;
    let table = checked_function_table(
        lua,
        native,
        &["data", "data_file", "map", "plugin_parameter", "open"],
    )?;
    table.set("DECODE_JSON", lua.create_userdata(LuaDecodeJsonString)?)?;
    Ok(table)
}

fn checked_function_table(lua: &Lua, native: Table, names: &[&str]) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    for name in names {
        let function: Function = native.get(*name)?;
        table.set(*name, checked_host_function(lua, function)?)?;
    }
    Ok(table)
}

fn checked_host_function(lua: &Lua, native: Function) -> mlua::Result<Function> {
    let factory: Function = lua
        .load(
            "return function(native) return function(...) local ok, value = native(...); if not ok then error(value, 0) end; return value end end",
        )
        .eval()?;
    factory.call(native)
}

fn parse_source_path(value: Value) -> Result<LuaSourcePath, TrustedLuaHostCallError> {
    let Value::String(value) = value else {
        return Err(binding_error(mlua::Error::runtime(format!(
            "来源路径必须是 UTF-8 字符串，实际为 {}",
            value.type_name()
        ))));
    };
    let value = lua_string_to_text(&value, "来源路径").map_err(binding_error)?;
    LuaSourcePath::parse(&value).map_err(|error| {
        TrustedLuaHostCallError::new(
            "binding",
            "invalid_source_path",
            error.to_string(),
            None,
            Some(Arc::new(error)),
        )
    })
}

fn parse_rpg_maker_string(value: Value, role: &str) -> Result<String, TrustedLuaHostCallError> {
    let Value::String(value) = value else {
        return Err(rpg_maker_argument_error(format!(
            "{role} 必须是 UTF-8 字符串，实际为 {}",
            value.type_name()
        )));
    };
    lua_string_to_text(&value, role).map_err(|error| rpg_maker_argument_error(error.to_string()))
}

fn parse_rpg_maker_integer(value: Value, role: &str) -> Result<i64, TrustedLuaHostCallError> {
    let Value::Integer(value) = value else {
        return Err(rpg_maker_argument_error(format!(
            "{role} 必须是 Lua integer，实际为 {}",
            value.type_name()
        )));
    };
    Ok(value)
}

fn parse_rpg_maker_source(value: Value) -> Result<RpgMakerSource, TrustedLuaHostCallError> {
    let Value::UserData(value) = value else {
        return Err(rpg_maker_argument_error(format!(
            "ctx.rpg_maker.open 只接受 Rust 建立的 RPG Maker Source，实际为 {}",
            value.type_name()
        )));
    };
    if !value.is::<LuaRpgMakerSource>() {
        return Err(rpg_maker_argument_error(
            "ctx.rpg_maker.open 只接受 Rust 建立的 RPG Maker Source".to_owned(),
        ));
    }
    value
        .borrow::<LuaRpgMakerSource>()
        .map(|source| source.0.clone())
        .map_err(|error| rpg_maker_argument_error(error.to_string()))
}

fn rpg_maker_argument_error(message: String) -> TrustedLuaHostCallError {
    TrustedLuaHostCallError::new("rpg_maker", "invalid_argument", message, None, None)
}

fn rpg_maker_host_error(error: RpgMakerDocumentError) -> TrustedLuaHostCallError {
    let kind = error.kind();
    let message = error.to_string();
    TrustedLuaHostCallError::new("rpg_maker", kind, message, None, Some(Arc::new(error)))
}

fn build_project_table(lua: &Lua, project: &LuaProjectContext) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("name", project.name())?;
    table.set("engine", project.engine().storage_name())?;
    table.set("source_root", strict_path(project.source_root())?)?;
    table.set("database_path", strict_path(project.database_path())?)?;
    table.set("source_language", project.source_language().as_str())?;
    table.set("target_language", project.target_language().as_str())?;
    match project.output_root() {
        Some(path) => table.set("output_root", strict_path(path)?)?,
        None => table.set("output_root", Value::Nil)?,
    }
    Ok(table)
}

fn strict_path(path: &Path) -> mlua::Result<&str> {
    path.to_str()
        .ok_or_else(|| mlua::Error::runtime("项目路径无法无损转换为 UTF-8"))
}

fn build_database_table(
    lua: &Lua,
    calls: Arc<dyn TrustedLuaCommonHostCalls>,
    tokio: Handle,
    cancellation: RuntimeCancellation,
) -> mlua::Result<Table> {
    let native = lua.create_table()?;
    let null = lua.create_userdata(LuaSqliteNull)?;

    let query_calls = Arc::clone(&calls);
    let query_tokio = tokio.clone();
    let query_cancellation = cancellation.clone();
    native.set(
        "query",
        lua.create_function(move |lua, (statement, parameters): (Value, Value)| {
            let result = parse_sql_call(statement, parameters)
                .and_then(|(statement, parameters)| {
                    wait_for_host(
                        &query_tokio,
                        &query_cancellation,
                        query_calls.query(SqliteQuery::new(statement, parameters)),
                    )
                })
                .map_err(|error| error.with_operation("db.query"));
            host_result_to_lua(lua, result, rows_to_lua)
        })?,
    )?;

    let execute_calls = Arc::clone(&calls);
    let execute_tokio = tokio.clone();
    let execute_cancellation = cancellation.clone();
    native.set(
        "execute",
        lua.create_function(move |lua, (statement, parameters): (Value, Value)| {
            let result = parse_sql_call(statement, parameters)
                .and_then(|(statement, parameters)| {
                    wait_for_host(
                        &execute_tokio,
                        &execute_cancellation,
                        execute_calls.execute(SqliteCommand::new(statement, parameters)),
                    )
                })
                .map_err(|error| error.with_operation("db.execute"));
            host_result_to_lua(lua, result, |_, changed| {
                i64::try_from(changed)
                    .map(Value::Integer)
                    .map_err(|_| mlua::Error::runtime("SQLite 受影响行数超出 Lua integer"))
            })
        })?,
    )?;

    for (name, operation) in [
        ("begin", DatabaseOperation::Begin),
        ("commit", DatabaseOperation::Commit),
        ("rollback", DatabaseOperation::Rollback),
    ] {
        let calls = Arc::clone(&calls);
        let tokio = tokio.clone();
        let cancellation = cancellation.clone();
        native.set(
            name,
            lua.create_function(move |lua, ()| {
                let future = match operation {
                    DatabaseOperation::Begin => calls.begin(),
                    DatabaseOperation::Commit => calls.commit(),
                    DatabaseOperation::Rollback => calls.rollback(),
                };
                host_result_to_lua(
                    lua,
                    wait_for_host(&tokio, &cancellation, future).map_err(|error| {
                        error.with_operation(match operation {
                            DatabaseOperation::Begin => "db.begin",
                            DatabaseOperation::Commit => "db.commit",
                            DatabaseOperation::Rollback => "db.rollback",
                        })
                    }),
                    |_, ()| Ok(Value::Nil),
                )
            })?,
        )?;
    }

    let blob = lua.create_function(move |lua, bytes: Value| {
        let result = match bytes {
            Value::String(bytes) => Ok(bytes.as_bytes().to_vec()),
            value => Err(binding_error(mlua::Error::runtime(format!(
                "ctx.db.blob 的参数必须是字符串，实际为 {}",
                value.type_name()
            )))),
        };
        host_result_to_lua(lua, result, |lua, bytes| {
            lua.create_userdata(LuaBlob(bytes)).map(Value::UserData)
        })
    })?;
    let factory: Function = lua
        .load(
            r#"
return function(native, null_value, native_blob)
  local function checked(call)
    return function(...)
      local ok, value = call(...)
      if not ok then error(value, 0) end
      return value
    end
  end
  return {
    NULL = null_value,
    blob = checked(native_blob),
    query = checked(native.query),
    execute = checked(native.execute),
    begin = checked(native.begin),
    commit = checked(native.commit),
    rollback = checked(native.rollback),
  }
end
"#,
        )
        .eval()?;
    factory.call((native, null, blob))
}

#[derive(Clone, Copy)]
enum DatabaseOperation {
    Begin,
    Commit,
    Rollback,
}

fn build_llm_function(
    lua: &Lua,
    calls: Arc<dyn TrustedLuaTranslateHostCalls>,
    tokio: Handle,
    cancellation: RuntimeCancellation,
) -> mlua::Result<Function> {
    let native = lua.create_function(move |lua, messages: Value| {
        let result = parse_message_array(messages)
            .and_then(|messages| wait_for_host(&tokio, &cancellation, calls.request_llm(messages)))
            .map_err(|error| error.with_operation("llm.request"));
        host_result_to_lua(lua, result, llm_response_to_lua)
    })?;
    let factory: Function = lua
        .load(
            "return function(native) return function(...) local ok, value = native(...); if not ok then error(value, 0) end; return value end end",
        )
        .eval()?;
    factory.call(native)
}

fn wait_for_host<T>(
    tokio: &Handle,
    cancellation: &RuntimeCancellation,
    future: std::pin::Pin<
        Box<dyn Future<Output = Result<T, TrustedLuaHostCallError>> + Send + 'static>,
    >,
) -> Result<T, TrustedLuaHostCallError>
where
    T: Send + 'static,
{
    let (sender, receiver) = mpsc::sync_channel(1);
    let task = tokio.spawn(async move {
        let _ = sender.send(future.await);
    });
    loop {
        match receiver.recv_timeout(std::time::Duration::from_millis(10)) {
            Ok(result) => return result,
            Err(mpsc::RecvTimeoutError::Timeout) if cancellation.is_cancelled() => {
                task.abort();
                return Err(TrustedLuaHostCallError::new(
                    "runtime",
                    "cancelled",
                    "Lua Host 调用已取消",
                    None,
                    None,
                ));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(TrustedLuaHostCallError::new(
                    "runtime",
                    "host_bridge_closed",
                    "Lua Host 响应桥在返回结果前关闭",
                    None,
                    None,
                ));
            }
        }
    }
}

/// 候选目录操作一旦交给根，就必须等到明确终态后才能让外层发布或丢弃候选。
///
/// 取消只改变交还给 Lua 的结果，不中止已经接管的根调用；这与普通只读 Host 调用可
/// 直接 abort 桥接任务的语义不同。
fn wait_for_output_terminal<T>(
    tokio: &Handle,
    cancellation: &RuntimeCancellation,
    future: std::pin::Pin<
        Box<dyn Future<Output = Result<T, TrustedLuaHostCallError>> + Send + 'static>,
    >,
) -> Result<T, TrustedLuaHostCallError>
where
    T: Send + 'static,
{
    let (sender, receiver) = mpsc::sync_channel(1);
    let _task = tokio.spawn(async move {
        let _ = sender.send(future.await);
    });
    let mut cancelled = cancellation.is_cancelled();
    loop {
        match receiver.recv_timeout(std::time::Duration::from_millis(10)) {
            Ok(_result) if cancelled => {
                return Err(TrustedLuaHostCallError::new(
                    "runtime",
                    "cancelled",
                    "Lua 候选目录调用已在到达明确终态后取消",
                    None,
                    None,
                ));
            }
            Ok(result) => return result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                cancelled |= cancellation.is_cancelled();
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(TrustedLuaHostCallError::new(
                    "runtime",
                    "host_bridge_closed",
                    "Lua 候选目录响应桥在返回明确终态前关闭",
                    None,
                    None,
                ));
            }
        }
    }
}

fn host_result_to_lua<T, F>(
    lua: &Lua,
    result: Result<T, TrustedLuaHostCallError>,
    success: F,
) -> mlua::Result<MultiValue>
where
    F: FnOnce(&Lua, T) -> mlua::Result<Value>,
{
    match result {
        Ok(value) => match success(lua, value) {
            Ok(value) => Ok(MultiValue::from_vec(vec![Value::Boolean(true), value])),
            Err(error) => host_error_to_lua(lua, binding_error(error)),
        },
        Err(error) => host_error_to_lua(lua, error),
    }
}

fn host_error_to_lua(lua: &Lua, error: TrustedLuaHostCallError) -> mlua::Result<MultiValue> {
    Ok(MultiValue::from_vec(vec![
        Value::Boolean(false),
        Value::UserData(lua.create_userdata(LuaHostErrorUserData(error))?),
    ]))
}

fn binding_error(error: mlua::Error) -> TrustedLuaHostCallError {
    let _ = error;
    TrustedLuaHostCallError::new(
        "binding",
        "invalid_value",
        "Lua Host 参数或返回值不符合绑定协议",
        None,
        None,
    )
}

fn parse_sql_call(
    statement: Value,
    parameters: Value,
) -> Result<(String, Vec<SqliteValue>), TrustedLuaHostCallError> {
    let statement = match statement {
        Value::String(statement) => lua_string_to_text(&statement, "SQL").map_err(binding_error)?,
        value => {
            return Err(binding_error(mlua::Error::runtime(format!(
                "SQL 必须是字符串，实际为 {}",
                value.type_name()
            ))));
        }
    };
    let parameters = parse_parameters(parameters)?;
    Ok((statement, parameters))
}

fn parse_parameters(value: Value) -> Result<Vec<SqliteValue>, TrustedLuaHostCallError> {
    match value {
        Value::Nil => Ok(Vec::new()),
        Value::Table(table) => dense_values(table, binding_error)?
            .into_iter()
            .map(|value| lua_to_sqlite_value(value).map_err(binding_error))
            .collect(),
        other => Err(binding_error(mlua::Error::runtime(format!(
            "SQLite parameters 必须是无洞数组或 nil，实际为 {}",
            other.type_name()
        )))),
    }
}

fn dense_table_values(table: Table) -> mlua::Result<Vec<Value>> {
    let mut indexed = Vec::new();
    for pair in table.pairs::<Value, Value>() {
        let (key, value) = pair?;
        let Value::Integer(index) = key else {
            return Err(mlua::Error::runtime("数组不得包含非整数键"));
        };
        let index = usize::try_from(index)
            .ok()
            .filter(|index| *index > 0)
            .ok_or_else(|| mlua::Error::runtime("数组下标必须从 1 开始"))?;
        indexed.push((index, value));
    }
    indexed.sort_by_key(|(index, _)| *index);
    for (offset, (index, _)) in indexed.iter().enumerate() {
        if *index != offset + 1 {
            return Err(mlua::Error::runtime("数组必须无洞且连续"));
        }
    }
    Ok(indexed.into_iter().map(|(_, value)| value).collect())
}

fn dense_values<F>(table: Table, invalid: F) -> Result<Vec<Value>, TrustedLuaHostCallError>
where
    F: FnOnce(mlua::Error) -> TrustedLuaHostCallError,
{
    dense_table_values(table).map_err(invalid)
}

fn lua_to_sqlite_value(value: Value) -> mlua::Result<SqliteValue> {
    match value {
        Value::Integer(value) => Ok(SqliteValue::Integer(value)),
        Value::Number(value) if value.is_finite() => Ok(SqliteValue::Real(value)),
        Value::Number(_) => Err(mlua::Error::runtime("SQLite REAL 参数不得为 NaN 或 Inf")),
        Value::String(value) => Ok(SqliteValue::Text(lua_string_to_text(&value, "TEXT")?)),
        Value::UserData(value) if value.is::<LuaSqliteNull>() => Ok(SqliteValue::Null),
        Value::UserData(value) if value.is::<LuaBlob>() => {
            Ok(SqliteValue::Blob(value.borrow::<LuaBlob>()?.0.clone()))
        }
        other => Err(mlua::Error::runtime(format!(
            "SQLite 参数不支持 {}",
            other.type_name()
        ))),
    }
}

fn rows_to_lua(lua: &Lua, rows: Vec<SqliteRow>) -> mlua::Result<Value> {
    let result = lua.create_table_with_capacity(rows.len(), 0)?;
    for (row_index, row) in rows.into_iter().enumerate() {
        let values = row.into_values();
        let row_table = lua.create_table_with_capacity(values.len(), 0)?;
        for (column_index, value) in values.into_iter().enumerate() {
            row_table.raw_set(column_index + 1, sqlite_to_lua_value(lua, value)?)?;
        }
        result.raw_set(row_index + 1, row_table)?;
    }
    Ok(Value::Table(result))
}

fn sqlite_to_lua_value(lua: &Lua, value: SqliteValue) -> mlua::Result<Value> {
    match value {
        SqliteValue::Null => Ok(Value::UserData(lua.create_userdata(LuaSqliteNull)?)),
        SqliteValue::Integer(value) => Ok(Value::Integer(value)),
        SqliteValue::Real(value) if value.is_finite() => Ok(Value::Number(value)),
        SqliteValue::Real(_) => Err(mlua::Error::runtime("SQLite REAL 结果为 NaN 或 Inf")),
        SqliteValue::Text(value) => Ok(Value::String(lua.create_string(value)?)),
        SqliteValue::Blob(value) => Ok(Value::UserData(lua.create_userdata(LuaBlob(value))?)),
    }
}

fn parse_messages(table: Table) -> Result<Vec<ChatMessage>, TrustedLuaHostCallError> {
    dense_values(table, binding_error)?
        .into_iter()
        .map(|value| {
            let Value::Table(message) = value else {
                return Err(binding_error(mlua::Error::runtime(
                    "LLM messages 的每一项必须是 table",
                )));
            };
            ensure_exact_string_keys(&message, &["role", "content"]).map_err(binding_error)?;
            let role: mlua::LuaString = message.get("role").map_err(binding_error)?;
            let content: mlua::LuaString = message.get("content").map_err(binding_error)?;
            let role = lua_string_to_text(&role, "message.role").map_err(binding_error)?;
            let content = lua_string_to_text(&content, "message.content").map_err(binding_error)?;
            let role = match role.as_str() {
                "system" => ChatMessageRole::System,
                "user" => ChatMessageRole::User,
                "assistant" => ChatMessageRole::Assistant,
                _ => return Err(binding_error(mlua::Error::runtime("LLM message.role 无效"))),
            };
            Ok(ChatMessage::new(role, content))
        })
        .collect()
}

fn parse_message_array(value: Value) -> Result<Vec<ChatMessage>, TrustedLuaHostCallError> {
    let Value::Table(messages) = value else {
        return Err(binding_error(mlua::Error::runtime(format!(
            "LLM messages 必须是无洞数组，实际为 {}",
            value.type_name()
        ))));
    };
    parse_messages(messages)
}

fn ensure_exact_string_keys(table: &Table, expected: &[&str]) -> mlua::Result<()> {
    let mut found = Vec::new();
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, _) = pair?;
        let Value::String(key) = key else {
            return Err(mlua::Error::runtime("table 字段名必须是字符串"));
        };
        let key: String = key.to_str()?.to_string();
        if !expected.iter().any(|expected| *expected == key) {
            return Err(mlua::Error::runtime(format!("table 包含未知字段 {key}")));
        }
        found.push(key);
    }
    for expected in expected {
        if !found.iter().any(|found| found == *expected) {
            return Err(mlua::Error::runtime(format!("table 缺少字段 {expected}")));
        }
    }
    Ok(())
}

fn llm_response_to_lua(lua: &Lua, response: LlmResponse) -> mlua::Result<Value> {
    let table = lua.create_table()?;
    table.set("content", response.content())?;
    table.set("finish_reason", response.finish_reason().to_string())?;
    match response.provider_request_id() {
        Some(request_id) => table.set("request_id", request_id)?,
        None => table.set("request_id", Value::Nil)?,
    }
    match response.provider_response_id() {
        Some(response_id) => table.set("response_id", response_id)?,
        None => table.set("response_id", Value::Nil)?,
    }
    match response.usage() {
        Some(usage) => table.set("usage", usage_to_lua(lua, usage)?)?,
        None => table.set("usage", Value::Nil)?,
    }
    Ok(Value::Table(table))
}

fn usage_to_lua(lua: &Lua, usage: LlmUsage) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("prompt_tokens", integer_token_count(usage.prompt_tokens())?)?;
    table.set(
        "completion_tokens",
        integer_token_count(usage.completion_tokens())?,
    )?;
    table.set("total_tokens", integer_token_count(usage.total_tokens())?)?;
    Ok(table)
}

fn integer_token_count(value: u64) -> mlua::Result<i64> {
    i64::try_from(value).map_err(|_| mlua::Error::runtime("LLM token 用量超出 Lua integer"))
}

fn lua_string_to_text(value: &mlua::LuaString, role: &str) -> mlua::Result<String> {
    value
        .to_str()
        .map(|value| value.to_owned())
        .map_err(|_| mlua::Error::runtime(format!("{role} 必须是 UTF-8 字符串")))
}

#[derive(Clone, Debug)]
struct LuaRpgMakerSource(RpgMakerSource);

impl UserData for LuaRpgMakerSource {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::Eq, |_lua, this, other: AnyUserData| {
            if !other.is::<LuaRpgMakerSource>() {
                return Ok(false);
            }
            Ok(other.borrow::<LuaRpgMakerSource>()?.0 == this.0)
        });
        methods.add_meta_method(MetaMethod::ToString, |_lua, this, ()| {
            Ok(this.0.to_string())
        });
    }
}

#[derive(Clone, Copy, Debug)]
struct LuaDecodeJsonString;

impl UserData for LuaDecodeJsonString {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::Eq, |_lua, _this, other: AnyUserData| {
            Ok(other.is::<LuaDecodeJsonString>())
        });
        methods.add_meta_method(MetaMethod::ToString, |_lua, _this, ()| Ok("DECODE_JSON"));
    }
}

#[derive(Clone)]
struct LuaRpgMakerDocument {
    document: OpenedRpgMakerDocument,
    markers: Table,
}

impl UserData for LuaRpgMakerDocument {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("value", |lua, this| {
            let document = this.document.clone();
            let markers = this.markers.clone();
            let native = lua.create_function(move |lua, (_document, path): (Value, Value)| {
                let result = parse_rpg_maker_path(path)
                    .and_then(|steps| document.value(&steps).map_err(rpg_maker_host_error))
                    .map_err(|error| error.with_operation("rpg_maker.document.value"));
                host_result_to_lua(lua, result, |lua, value| {
                    lossless_json_to_lua(lua, value, &markers)
                })
            })?;
            checked_host_function(lua, native)
        });
        fields.add_field_method_get("location", |lua, this| {
            let document = this.document.clone();
            let native = lua.create_function(move |lua, (_document, path): (Value, Value)| {
                let result = parse_rpg_maker_path(path)
                    .and_then(|steps| document.location(&steps).map_err(rpg_maker_host_error))
                    .map_err(|error| error.with_operation("rpg_maker.document.location"));
                host_result_to_lua(lua, result, |lua, location| {
                    lua.create_userdata(LuaRpgMakerLocation(location))
                        .map(Value::UserData)
                })
            })?;
            checked_host_function(lua, native)
        });
        fields.add_field_method_get("text", |lua, this| {
            let document = this.document.clone();
            let native = lua.create_function(move |lua, (_document, path): (Value, Value)| {
                let result = parse_rpg_maker_path(path)
                    .and_then(|steps| document.text(&steps).map_err(rpg_maker_host_error))
                    .map_err(|error| error.with_operation("rpg_maker.document.text"));
                host_result_to_lua(lua, result, |lua, reference| {
                    lua.create_userdata(LuaRpgMakerTextReference(reference))
                        .map(Value::UserData)
                })
            })?;
            checked_host_function(lua, native)
        });
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::ToString, |_lua, this, ()| {
            Ok(format!("RpgMakerDocument({})", this.document.source()))
        });
    }
}

#[derive(Clone, Debug)]
struct LuaRpgMakerLocation(RpgMakerLocation);

impl UserData for LuaRpgMakerLocation {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::Eq, |_lua, this, other: AnyUserData| {
            if !other.is::<LuaRpgMakerLocation>() {
                return Ok(false);
            }
            Ok(other.borrow::<LuaRpgMakerLocation>()?.0 == this.0)
        });
        methods.add_meta_method(MetaMethod::ToString, |_lua, this, ()| {
            Ok(this.0.to_string())
        });
    }
}

#[derive(Clone, Debug)]
struct LuaRpgMakerTextReference(RpgMakerTextReference);

impl UserData for LuaRpgMakerTextReference {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("original", |_lua, this| Ok(this.0.original().to_owned()));
        fields.add_field_method_get("location", |lua, this| {
            lua.create_userdata(LuaRpgMakerLocation(this.0.location().clone()))
        });
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::ToString, |_lua, this, ()| {
            Ok(this.0.original().to_owned())
        });
    }
}

fn parse_rpg_maker_path(path: Value) -> Result<Vec<RpgMakerLocationStep>, TrustedLuaHostCallError> {
    let Value::Table(path) = path else {
        return Err(rpg_maker_argument_error(format!(
            "RPG Maker path 必须是无洞数组，实际为 {}",
            path.type_name()
        )));
    };
    dense_values(path, |error| rpg_maker_argument_error(error.to_string()))?
        .into_iter()
        .map(|value| match value {
            Value::String(key) => {
                let key = lua_string_to_text(&key, "RPG Maker object key")
                    .map_err(|error| rpg_maker_argument_error(error.to_string()))?;
                Ok(RpgMakerLocationStep::key(key))
            }
            Value::Integer(index) => usize::try_from(index)
                .map(RpgMakerLocationStep::index)
                .map_err(|_| {
                    rpg_maker_argument_error(
                        "RPG Maker array index 必须是非负 Lua integer".to_owned(),
                    )
                }),
            Value::UserData(value) if value.is::<LuaDecodeJsonString>() => {
                Ok(RpgMakerLocationStep::DecodeJsonString)
            }
            value => Err(rpg_maker_argument_error(format!(
                "RPG Maker path 只接受字符串、非负整数或 ctx.rpg_maker.DECODE_JSON，实际为 {}",
                value.type_name()
            ))),
        })
        .collect()
}

#[derive(Clone, Copy, Debug)]
struct LuaJsonNull;

impl UserData for LuaJsonNull {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::Eq, |_lua, _this, other: AnyUserData| {
            Ok(other.is::<LuaJsonNull>())
        });
    }
}

#[derive(Clone, Copy, Debug)]
struct LuaSqliteNull;

impl UserData for LuaSqliteNull {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::Eq, |_lua, _this, other: AnyUserData| {
            Ok(other.is::<LuaSqliteNull>())
        });
    }
}

#[derive(Clone, Debug)]
struct LuaJsonNumber(String);

impl UserData for LuaJsonNumber {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::Eq, |_lua, this, other: AnyUserData| {
            if !other.is::<LuaJsonNumber>() {
                return Ok(false);
            }
            Ok(other.borrow::<LuaJsonNumber>()?.0 == this.0)
        });
        methods.add_meta_method(MetaMethod::ToString, |_lua, this, ()| Ok(this.0.clone()));
    }
}

#[derive(Clone, Debug)]
struct LuaBlob(Vec<u8>);

impl UserData for LuaBlob {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("bytes", |lua, this, ()| lua.create_string(&this.0));
    }
}

#[derive(Clone, Debug)]
struct LuaHostErrorUserData(TrustedLuaHostCallError);

impl UserData for LuaHostErrorUserData {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("domain", |_lua, this| Ok(this.0.domain()));
        fields.add_field_method_get("kind", |_lua, this| Ok(this.0.kind()));
        fields.add_field_method_get("operation", |_lua, this| Ok(this.0.operation()));
        fields.add_field_method_get("message", |_lua, this| Ok(this.0.message().to_owned()));
        fields.add_field_method_get("retry_after_ms", |_lua, this| {
            this.0
                .retry_after_ms()
                .map(i64::try_from)
                .transpose()
                .map_err(|_| mlua::Error::runtime("retry_after_ms 超出 Lua integer"))
        });
    }
}

fn vm_error(
    operation: &'static str,
    context: &str,
    error: mlua::Error,
) -> TrustedLua54RuntimeError {
    TrustedLua54RuntimeError::Vm {
        operation,
        message: format!("{context}：{error}"),
    }
}

fn lua_value_description(value: &Value) -> String {
    match value {
        Value::String(value) => value
            .to_str()
            .map(|value| value.to_owned())
            .unwrap_or_else(|_| "<non-UTF-8 Lua error>".to_owned()),
        other => format!("Lua {} 错误值", other.type_name()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::path::PathBuf;
    use std::time::Duration;

    use crate::rpg_maker::ProjectName;
    use crate::rpg_maker::lua::runtime::{
        TrustedLuaBindingFinalization, TrustedLuaBindingFinalizer,
        TrustedLuaManagedTranslationResultStatus, TrustedLuaManagedTranslationUnitStatus,
        TrustedLuaOutputEntry, TrustedLuaPreparedTranslationStatus, TrustedLuaRuntimeBindings,
        TrustedLuaStandardExtractIntent, TrustedLuaStandardLinePolicy,
        TrustedLuaStandardRejectionDetail, TrustedLuaStandardUnitStatus,
        TrustedLuaTranslationSemantics, TrustedLuaTranslationTerm,
    };
    use crate::rpg_maker::project::OpenedProject;
    use crate::runtime::sqlite::{
        RusqliteInteractiveSessionFinalizer, RusqliteInteractiveSessionOperations, RusqliteStorage,
        RusqliteStorageConfiguration,
    };
    use crate::storage::sqlite_session::{
        SqliteInteractiveSessionError, SqliteInteractiveSessionFactory,
        SqliteInteractiveSessionFinalizer, SqliteInteractiveSessionOperations,
    };

    #[derive(Default)]
    struct TestObservations {
        executed_parameters: Vec<SqliteValue>,
        messages: Vec<ChatMessage>,
        prepared: Vec<(TextGroupKind, String, String)>,
        extract_intents: Vec<TrustedLuaStandardExtractIntent>,
        managed_snapshots: Vec<TrustedLuaManagedTranslationSnapshot>,
        managed_translate_calls: usize,
        managed_open_names: Vec<String>,
        output_operations: Vec<String>,
        output_writes: Vec<(String, Vec<u8>)>,
        standard_candidates: Vec<TrustedLuaStandardCandidate>,
        layouts: Vec<(
            TrustedLuaWriteBackLayoutRegion,
            Vec<TrustedLuaWriteBackLayoutPair>,
        )>,
    }

    struct TestCalls {
        panic_on_project: bool,
        project: LuaProjectContext,
        observations: Arc<Mutex<TestObservations>>,
        begin_error: Option<TrustedLuaHostCallError>,
        begin_started: Option<Arc<Notify>>,
        begin_gate: Option<Arc<Notify>>,
    }

    fn test_managed_collection(name: String) -> TrustedLuaManagedTranslationCollection {
        TrustedLuaManagedTranslationCollection::new(
            name,
            "翻译任务标题；保持简洁。".to_owned(),
            vec![
                TrustedLuaManagedTranslationUnit::new(
                    "single".to_owned(),
                    "plugin_parameter".to_owned(),
                    TrustedLuaManagedTranslationShape::Single,
                    TrustedLuaManagedTranslationContent::scalar("星港へ"),
                    "任务标题".to_owned(),
                    Some(r#"{"quest_id":12,"tag":"main"}"#.to_owned()),
                    Some(TrustedLuaManagedTranslationContent::scalar("前往星港")),
                    TrustedLuaManagedTranslationUnitStatus::Current,
                ),
                TrustedLuaManagedTranslationUnit::new(
                    "reflow".to_owned(),
                    "plugin_parameter".to_owned(),
                    TrustedLuaManagedTranslationShape::Reflow,
                    TrustedLuaManagedTranslationContent::scalar("長い説明"),
                    String::new(),
                    Some(r#"[1,"tag"]"#.to_owned()),
                    Some(TrustedLuaManagedTranslationContent::scalar("很长的\n说明")),
                    TrustedLuaManagedTranslationUnitStatus::Current,
                ),
                TrustedLuaManagedTranslationUnit::new(
                    "lines".to_owned(),
                    "map".to_owned(),
                    TrustedLuaManagedTranslationShape::Lines,
                    TrustedLuaManagedTranslationContent::array(vec![
                        "第一行".to_owned(),
                        String::new(),
                    ]),
                    String::new(),
                    Some(r#""tag""#.to_owned()),
                    Some(TrustedLuaManagedTranslationContent::array(vec![
                        "第 1 行".to_owned(),
                        String::new(),
                    ])),
                    TrustedLuaManagedTranslationUnitStatus::Current,
                ),
                TrustedLuaManagedTranslationUnit::new(
                    "items".to_owned(),
                    "choices".to_owned(),
                    TrustedLuaManagedTranslationShape::Items,
                    TrustedLuaManagedTranslationContent::array(vec![
                        "はい".to_owned(),
                        "いいえ".to_owned(),
                    ]),
                    String::new(),
                    Some("null".to_owned()),
                    None,
                    TrustedLuaManagedTranslationUnitStatus::Unavailable,
                ),
            ],
        )
    }

    fn test_managed_report() -> TrustedLuaManagedTranslationReport {
        TrustedLuaManagedTranslationReport::new(vec![
            TrustedLuaManagedTranslationResult::new(
                "quest_titles".to_owned(),
                "single".to_owned(),
                TrustedLuaManagedTranslationResultStatus::Translated,
                Some(TrustedLuaManagedTranslationContent::scalar("前往星港")),
                None,
                Some(r#"{"changed_locations":2}"#.to_owned()),
            ),
            TrustedLuaManagedTranslationResult::new(
                "quest_titles".to_owned(),
                "items".to_owned(),
                TrustedLuaManagedTranslationResultStatus::Unavailable,
                None,
                Some("placeholder_mismatch".to_owned()),
                Some(r#"{"index":2}"#.to_owned()),
            ),
        ])
    }

    impl TrustedLuaCommonHostCalls for TestCalls {
        fn project(&self) -> &LuaProjectContext {
            assert!(!self.panic_on_project, "测试请求 Host project panic");
            &self.project
        }

        fn read_source(
            &self,
            path: LuaSourcePath,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<Vec<u8>, TrustedLuaHostCallError>> + Send + 'static>,
        > {
            Box::pin(async move {
                match path.as_str() {
                    "data/Items.json" => Ok(
                        r#"[null,{"name":"药水","note":"<Help:恢复 HP>","nested":"[{\"Title\":\"任务\"}]"}]"#
                            .as_bytes()
                            .to_vec(),
                    ),
                    "data/Map001.json" => Ok(
                        r#"{"list":[{"code":108,"indent":0,"parameters":["<Quest:第一"]},{"code":408,"indent":0,"parameters":["行>"]},{"code":0,"indent":0,"parameters":[]}]}"#
                            .as_bytes()
                            .to_vec(),
                    ),
                    "js/plugins.js" => Ok(
                        r#"var $plugins = [{"name":"Quest","parameters":{"Entries":"[{\"Title\":\"插件任务\"}]"}}];"#
                            .as_bytes()
                            .to_vec(),
                    ),
                    "js/raw.bin" => Ok(vec![0, 255, 1]),
                    "js/text.txt" => Ok("文本".as_bytes().to_vec()),
                    "js/value.json" => Ok(br#"{"large":1e999,"array":[]}"#.to_vec()),
                    other => Err(TrustedLuaHostCallError::new(
                        "filesystem",
                        "not_found",
                        format!("测试来源不存在：{other}"),
                        None,
                        None,
                    )),
                }
            })
        }

        fn list_source(
            &self,
            path: LuaSourcePath,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<Vec<String>, TrustedLuaHostCallError>> + Send + 'static>,
        > {
            Box::pin(async move {
                match path.as_str() {
                    "data" => Ok(vec![
                        "data/Items.json".to_owned(),
                        "data/Map001.json".to_owned(),
                    ]),
                    other => Err(TrustedLuaHostCallError::new(
                        "filesystem",
                        "not_found",
                        format!("测试目录不存在：{other}"),
                        None,
                        None,
                    )),
                }
            })
        }

        fn query(
            &self,
            query: SqliteQuery,
        ) -> std::pin::Pin<
            Box<
                dyn Future<Output = Result<Vec<SqliteRow>, TrustedLuaHostCallError>>
                    + Send
                    + 'static,
            >,
        > {
            Box::pin(async move {
                assert_eq!(query.statement(), "SELECT values");
                assert_eq!(
                    query.parameters(),
                    &[
                        SqliteValue::Null,
                        SqliteValue::Blob(vec![0, 255]),
                        SqliteValue::Text("input".to_owned()),
                        SqliteValue::Integer(9),
                        SqliteValue::Real(2.5),
                    ]
                );
                Ok(vec![SqliteRow::new(vec![
                    SqliteValue::Null,
                    SqliteValue::Integer(7),
                    SqliteValue::Real(1.5),
                    SqliteValue::Text("文本".to_owned()),
                    SqliteValue::Blob(vec![0, 255]),
                ])])
            })
        }

        fn execute(
            &self,
            command: SqliteCommand,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<u64, TrustedLuaHostCallError>> + Send + 'static>,
        > {
            let observations = Arc::clone(&self.observations);
            Box::pin(async move {
                assert_eq!(command.statement(), "INSERT values");
                observations
                    .lock()
                    .expect("测试记录锁不应中毒")
                    .executed_parameters = command.parameters().to_vec();
                Ok(1)
            })
        }

        fn begin(
            &self,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<(), TrustedLuaHostCallError>> + Send + 'static>,
        > {
            let error = self.begin_error.clone();
            let started = self.begin_started.as_ref().map(Arc::clone);
            let gate = self.begin_gate.as_ref().map(Arc::clone);
            Box::pin(async move {
                if let Some(started) = started {
                    started.notify_one();
                }
                if let Some(gate) = gate {
                    gate.notified().await;
                }
                error.map_or(Ok(()), Err)
            })
        }

        fn commit(
            &self,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<(), TrustedLuaHostCallError>> + Send + 'static>,
        > {
            Box::pin(async { Ok(()) })
        }

        fn rollback(
            &self,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<(), TrustedLuaHostCallError>> + Send + 'static>,
        > {
            Box::pin(async { Ok(()) })
        }

        fn transaction_active(
            &self,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<bool, TrustedLuaHostCallError>> + Send + 'static>,
        > {
            Box::pin(async { Ok(false) })
        }
    }

    impl TrustedLuaTranslateHostCalls for TestCalls {
        fn system_prompt(&self) -> &str {
            "只输出译文"
        }

        fn source_language(&self) -> &str {
            "ja"
        }

        fn target_language(&self) -> &str {
            "zh-Hans"
        }

        fn prepare_translation(
            &self,
            kind: TextGroupKind,
            original: String,
            semantic_context: String,
        ) -> Result<Arc<dyn TrustedLuaPreparedTranslation>, TrustedLuaHostCallError> {
            self.observations
                .lock()
                .expect("测试记录锁不应中毒")
                .prepared
                .push((kind, original.clone(), semantic_context));
            if original == "__prepare_error__" {
                return Err(TrustedLuaHostCallError::new(
                    "translation",
                    "prepare_failed",
                    "测试预处理技术错误",
                    None,
                    None,
                ));
            }
            let status = match original.as_str() {
                "__non_source__" => TrustedLuaPreparedTranslationStatus::NonSourceLanguage,
                "__fully_protected__" => TrustedLuaPreparedTranslationStatus::FullyProtected,
                _ => TrustedLuaPreparedTranslationStatus::Active,
            };
            let terms = if status == TrustedLuaPreparedTranslationStatus::Active {
                vec![
                    TrustedLuaTranslationTerm::new("勇者", "Hero"),
                    TrustedLuaTranslationTerm::new("魔王", "Demon King"),
                ]
            } else {
                Vec::new()
            };
            Ok(Arc::new(TestPreparedTranslation {
                status,
                model_text: if status == TrustedLuaPreparedTranslationStatus::Active {
                    "⟦ATT_COLOR_WHOLE_0000⟧勇者".to_owned()
                } else {
                    String::new()
                },
                terms,
            }))
        }

        fn request_llm(
            &self,
            messages: Vec<ChatMessage>,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<LlmResponse, TrustedLuaHostCallError>> + Send + 'static>,
        > {
            self.observations
                .lock()
                .expect("测试记录锁不应中毒")
                .messages = messages;
            Box::pin(async {
                Ok(LlmResponse::new(
                    "raw response",
                    crate::llm::LlmFinishReason::Stop,
                    Some("request-1".to_owned()),
                    Some("response-1".to_owned()),
                    Some(LlmUsage::new(3, 5, 8)),
                ))
            })
        }

        fn translate_managed(
            &self,
        ) -> std::pin::Pin<
            Box<
                dyn Future<
                        Output = Result<
                            TrustedLuaManagedTranslationReport,
                            TrustedLuaHostCallError,
                        >,
                    > + Send
                    + 'static,
            >,
        > {
            self.observations
                .lock()
                .expect("测试记录锁不应中毒")
                .managed_translate_calls += 1;
            Box::pin(async { Ok(test_managed_report()) })
        }

        fn open_managed(
            &self,
            name: String,
        ) -> std::pin::Pin<
            Box<
                dyn Future<
                        Output = Result<
                            Option<TrustedLuaManagedTranslationCollection>,
                            TrustedLuaHostCallError,
                        >,
                    > + Send
                    + 'static,
            >,
        > {
            self.observations
                .lock()
                .expect("测试记录锁不应中毒")
                .managed_open_names
                .push(name.clone());
            Box::pin(async move {
                if name == "missing" {
                    Ok(None)
                } else {
                    Ok(Some(test_managed_collection(name)))
                }
            })
        }
    }

    struct TestPreparedTranslation {
        status: TrustedLuaPreparedTranslationStatus,
        model_text: String,
        terms: Vec<TrustedLuaTranslationTerm>,
    }

    impl TrustedLuaPreparedTranslation for TestPreparedTranslation {
        fn status(&self) -> TrustedLuaPreparedTranslationStatus {
            self.status
        }

        fn model_text(&self) -> &str {
            &self.model_text
        }

        fn terms(&self) -> &[TrustedLuaTranslationTerm] {
            &self.terms
        }

        fn semantic_fingerprint(&self) -> Sha256Fingerprint {
            Sha256Fingerprint::from_bytes([0x5c; SHA256_FINGERPRINT_BYTES])
        }

        fn is_current(
            &self,
            translation: String,
            state: Sha256Fingerprint,
        ) -> Result<bool, TrustedLuaHostCallError> {
            Ok(translation == r"勇者\C[2]"
                && state == Sha256Fingerprint::from_bytes([0xab; SHA256_FINGERPRINT_BYTES]))
        }

        fn accept(
            &self,
            candidate: String,
        ) -> Result<TrustedLuaPreparedTranslationAcceptance, TrustedLuaHostCallError> {
            match candidate.as_str() {
                "bad" => Ok(TrustedLuaPreparedTranslationAcceptance::rejected(
                    "source_language_residual",
                )),
                "__accept_error__" => Err(TrustedLuaHostCallError::new(
                    "translation",
                    "accept_failed",
                    "测试验收技术错误",
                    None,
                    None,
                )),
                _ => Ok(TrustedLuaPreparedTranslationAcceptance::accepted(
                    format!("{candidate}\\C[2]"),
                    Sha256Fingerprint::from_bytes([0xab; SHA256_FINGERPRINT_BYTES]),
                )),
            }
        }
    }

    impl TrustedLuaExtractHostCalls for TestCalls {
        fn replace_standard(&self, snapshot: LuaSnapshot) -> Result<(), TrustedLuaHostCallError> {
            self.observations
                .lock()
                .expect("测试观察锁不应中毒")
                .extract_intents
                .push(TrustedLuaStandardExtractIntent::Replace(snapshot));
            Ok(())
        }

        fn clear_standard(&self) -> Result<(), TrustedLuaHostCallError> {
            self.observations
                .lock()
                .expect("测试观察锁不应中毒")
                .extract_intents
                .push(TrustedLuaStandardExtractIntent::Deactivate);
            Ok(())
        }

        fn replace_managed(
            &self,
            snapshot: TrustedLuaManagedTranslationSnapshot,
        ) -> Result<(), TrustedLuaHostCallError> {
            self.observations
                .lock()
                .expect("测试观察锁不应中毒")
                .managed_snapshots
                .push(snapshot);
            Ok(())
        }
    }

    impl TrustedLuaWriteBackHostCalls for TestCalls {
        fn open_managed(
            &self,
            name: String,
        ) -> std::pin::Pin<
            Box<
                dyn Future<
                        Output = Result<
                            Option<TrustedLuaManagedTranslationCollection>,
                            TrustedLuaHostCallError,
                        >,
                    > + Send
                    + 'static,
            >,
        > {
            self.observations
                .lock()
                .expect("测试记录锁不应中毒")
                .managed_open_names
                .push(name.clone());
            Box::pin(async move {
                if name == "missing" {
                    Ok(None)
                } else {
                    Ok(Some(test_managed_collection(name)))
                }
            })
        }

        fn read_output(
            &self,
            path: ScopedDirectoryPath,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<Vec<u8>, TrustedLuaHostCallError>> + Send + 'static>,
        > {
            let path = path.as_path().to_string_lossy().replace('\\', "/");
            Box::pin(async move {
                match path.as_str() {
                    "data/raw.bin" => Ok(vec![0, 255, 1]),
                    "data/text.txt" => Ok("文本".as_bytes().to_vec()),
                    "data/value.json" => Ok(br#"{"large":1e999,"array":[]}"#.to_vec()),
                    _ => Err(TrustedLuaHostCallError::new(
                        "output",
                        "not_found",
                        format!("测试候选文件不存在：{path}"),
                        None,
                        None,
                    )),
                }
            })
        }

        fn list_output(
            &self,
            path: ScopedDirectoryPath,
        ) -> std::pin::Pin<
            Box<
                dyn Future<Output = Result<Vec<TrustedLuaOutputEntry>, TrustedLuaHostCallError>>
                    + Send
                    + 'static,
            >,
        > {
            let path = path.as_path().to_string_lossy().replace('\\', "/");
            Box::pin(async move {
                if path != "data" {
                    return Err(TrustedLuaHostCallError::new(
                        "output",
                        "not_found",
                        format!("测试候选目录不存在：{path}"),
                        None,
                        None,
                    ));
                }
                Ok(vec![
                    TrustedLuaOutputEntry::new(
                        "nested".to_owned(),
                        crate::rpg_maker::lua::runtime::TrustedLuaOutputEntryKind::Directory,
                    ),
                    TrustedLuaOutputEntry::new(
                        "text.txt".to_owned(),
                        crate::rpg_maker::lua::runtime::TrustedLuaOutputEntryKind::File,
                    ),
                ])
            })
        }

        fn create_output_directory(
            &self,
            path: ScopedDirectoryPath,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<(), TrustedLuaHostCallError>> + Send + 'static>,
        > {
            let observations = Arc::clone(&self.observations);
            let path = path.as_path().to_string_lossy().replace('\\', "/");
            Box::pin(async move {
                observations
                    .lock()
                    .expect("测试观察锁不应中毒")
                    .output_operations
                    .push(format!("create:{path}"));
                Ok(())
            })
        }

        fn write_output(
            &self,
            path: ScopedDirectoryPath,
            bytes: Vec<u8>,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<(), TrustedLuaHostCallError>> + Send + 'static>,
        > {
            let observations = Arc::clone(&self.observations);
            let path = path.as_path().to_string_lossy().replace('\\', "/");
            let started = self.begin_started.clone();
            let gate = self.begin_gate.clone();
            Box::pin(async move {
                if path == "data/gated.bin" {
                    if let Some(started) = started {
                        started.notify_waiters();
                    }
                    if let Some(gate) = gate {
                        gate.notified().await;
                    }
                }
                observations
                    .lock()
                    .expect("测试观察锁不应中毒")
                    .output_writes
                    .push((path, bytes));
                Ok(())
            })
        }

        fn remove_output(
            &self,
            path: ScopedDirectoryPath,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<(), TrustedLuaHostCallError>> + Send + 'static>,
        > {
            let observations = Arc::clone(&self.observations);
            let path = path.as_path().to_string_lossy().replace('\\', "/");
            Box::pin(async move {
                observations
                    .lock()
                    .expect("测试观察锁不应中毒")
                    .output_operations
                    .push(format!("remove:{path}"));
                Ok(())
            })
        }

        fn layout(
            &self,
            region: TrustedLuaWriteBackLayoutRegion,
            pairs: Vec<TrustedLuaWriteBackLayoutPair>,
        ) -> Result<TrustedLuaWriteBackLayoutResult, TrustedLuaHostCallError> {
            self.observations
                .lock()
                .expect("测试观察锁不应中毒")
                .layouts
                .push((region, pairs.clone()));
            Ok(TrustedLuaWriteBackLayoutResult::new(
                crate::rpg_maker::lua::runtime::TrustedLuaWriteBackLayoutStatus::Applied,
                pairs
                    .iter()
                    .map(|pair| pair.translation().unwrap_or(pair.original()).to_owned())
                    .collect(),
                1,
                2,
            ))
        }
    }

    struct TestStandardCalls {
        session: Arc<TestStandardSession>,
    }

    impl TrustedLuaStandardHostCalls for TestStandardCalls {
        fn open(
            &self,
        ) -> std::pin::Pin<
            Box<
                dyn Future<
                        Output = Result<
                            Arc<dyn TrustedLuaStandardSession>,
                            TrustedLuaHostCallError,
                        >,
                    > + Send
                    + 'static,
            >,
        > {
            let session: Arc<dyn TrustedLuaStandardSession> = self.session.clone();
            Box::pin(async move { Ok(session) })
        }
    }

    struct TestStandardSession {
        units: Vec<TrustedLuaStandardUnit>,
        observations: Arc<Mutex<TestObservations>>,
    }

    impl TrustedLuaStandardSession for TestStandardSession {
        fn units(&self) -> Vec<TrustedLuaStandardUnit> {
            self.units.clone()
        }

        fn get(
            &self,
            owner: RpgMakerStandardAssetOwner,
            group_location: RpgMakerLocation,
            role: TextUnitRole,
        ) -> Option<TrustedLuaStandardUnit> {
            self.units
                .iter()
                .find(|unit| {
                    unit.owner() == owner
                        && unit.group_location() == &group_location
                        && unit.role() == &role
                })
                .cloned()
        }

        fn accept(
            &self,
            candidates: Vec<TrustedLuaStandardCandidate>,
        ) -> std::pin::Pin<
            Box<
                dyn Future<
                        Output = Result<Vec<TrustedLuaStandardAcceptance>, TrustedLuaHostCallError>,
                    > + Send
                    + 'static,
            >,
        > {
            self.observations
                .lock()
                .expect("测试观察锁不应中毒")
                .standard_candidates = candidates.clone();
            Box::pin(async move {
                Ok(candidates
                    .into_iter()
                    .map(|candidate| {
                        if candidate.handle() == 0 {
                            TrustedLuaStandardAcceptance::accepted(candidate.candidate().clone(), 2)
                        } else {
                            TrustedLuaStandardAcceptance::rejected(
                                "source_residual",
                                vec![TrustedLuaStandardRejectionDetail::new(
                                    "line",
                                    TrustedLuaStandardRejectionValue::Integer(2),
                                )],
                            )
                        }
                    })
                    .collect())
            })
        }
    }

    struct TestFinalizer {
        finalizations: Arc<Mutex<Vec<()>>>,
        completion: Option<oneshot::Sender<()>>,
    }

    impl TrustedLuaBindingFinalizer for TestFinalizer {
        fn finalize(
            self: Box<Self>,
        ) -> std::pin::Pin<
            Box<
                dyn Future<
                        Output = Result<
                            TrustedLuaBindingFinalization,
                            TrustedLuaBindingFinalizationError,
                        >,
                    > + Send
                    + 'static,
            >,
        > {
            let Self {
                finalizations,
                completion,
            } = *self;
            Box::pin(async move {
                finalizations.lock().expect("终结记录锁不应中毒").push(());
                if let Some(completion) = completion {
                    let _ = completion.send(());
                }
                Ok(TrustedLuaBindingFinalization::new(false))
            })
        }
    }

    struct PanickingFinalizer;

    impl TrustedLuaBindingFinalizer for PanickingFinalizer {
        fn finalize(
            self: Box<Self>,
        ) -> std::pin::Pin<
            Box<
                dyn Future<
                        Output = Result<
                            TrustedLuaBindingFinalization,
                            TrustedLuaBindingFinalizationError,
                        >,
                    > + Send
                    + 'static,
            >,
        > {
            panic!("测试请求 finalizer panic")
        }
    }

    fn test_configuration() -> TrustedLua54RuntimeConfiguration {
        TrustedLua54RuntimeConfiguration::new(
            NonZeroUsize::new(2 * 1024 * 1024).unwrap(),
            NonZeroU32::new(100).unwrap(),
        )
    }

    fn test_project() -> LuaProjectContext {
        let project = OpenedProject::new(
            "demo".parse::<ProjectName>().unwrap(),
            PathBuf::from(r"C:\projects\demo"),
            PathBuf::from(r"C:\projects\demo\project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            crate::rpg_maker::project::test_layout_profile(),
        );
        LuaProjectContext::for_frozen_source(
            project.name().as_str(),
            project.layout().rpg_maker_layout().engine(),
            project.source_root().to_path_buf(),
            project.database_path().to_path_buf(),
            project.language_pair().clone(),
        )
    }

    fn test_bindings(
        begin_error: Option<TrustedLuaHostCallError>,
        observations: Arc<Mutex<TestObservations>>,
        finalizations: Arc<Mutex<Vec<()>>>,
        completion: Option<oneshot::Sender<()>>,
    ) -> TrustedLuaRuntimeBindings {
        let calls = Arc::new(TestCalls {
            panic_on_project: false,
            project: test_project(),
            observations,
            begin_error,
            begin_started: None,
            begin_gate: None,
        });
        translate_bindings(
            calls,
            Box::new(TestFinalizer {
                finalizations,
                completion,
            }),
        )
    }

    fn common_bindings(calls: &Arc<TestCalls>) -> TrustedLuaCommonBindings {
        let calls: Arc<dyn TrustedLuaCommonHostCalls> = calls.clone();
        TrustedLuaCommonBindings::new(calls)
    }

    fn extract_bindings(
        calls: Arc<TestCalls>,
        finalizer: Box<dyn TrustedLuaBindingFinalizer>,
    ) -> TrustedLuaRuntimeBindings {
        let common = common_bindings(&calls);
        let extract: Arc<dyn TrustedLuaExtractHostCalls> = calls;
        TrustedLuaRuntimeBindings::extract(common, extract, finalizer)
    }

    fn test_extract_bindings(
        observations: Arc<Mutex<TestObservations>>,
    ) -> TrustedLuaRuntimeBindings {
        let calls = Arc::new(TestCalls {
            panic_on_project: false,
            project: test_project(),
            observations,
            begin_error: None,
            begin_started: None,
            begin_gate: None,
        });
        extract_bindings(
            calls,
            Box::new(TestFinalizer {
                finalizations: Arc::new(Mutex::new(Vec::new())),
                completion: None,
            }),
        )
    }

    async fn run_extract_program(
        runtime: &TrustedLua54Runtime,
        observations: Arc<Mutex<TestObservations>>,
        source: &str,
    ) -> Result<(), TrustedLuaRuntimeExecutionError<TrustedLua54RuntimeError>> {
        let report = runtime
            .start(
                OwnedLuaProgram::new(
                    PathBuf::from("C:/scripts/extract-managed.lua"),
                    source.as_bytes().to_vec(),
                ),
                test_extract_bindings(observations),
            )
            .await;
        let (runtime, finalization) = report.into_parts();
        finalization.expect("测试 SQLite finalizer 应成功");
        runtime
    }

    fn translate_bindings(
        calls: Arc<TestCalls>,
        finalizer: Box<dyn TrustedLuaBindingFinalizer>,
    ) -> TrustedLuaRuntimeBindings {
        let common = common_bindings(&calls);
        let translate: Arc<dyn TrustedLuaTranslateHostCalls> = calls;
        TrustedLuaRuntimeBindings::translate(common, translate, finalizer)
    }

    fn write_back_bindings(
        calls: Arc<TestCalls>,
        finalizer: Box<dyn TrustedLuaBindingFinalizer>,
    ) -> TrustedLuaRuntimeBindings {
        let common = common_bindings(&calls);
        let write_back: Arc<dyn TrustedLuaWriteBackHostCalls> = calls;
        TrustedLuaRuntimeBindings::write_back(common, write_back, finalizer)
    }

    fn project_bindings(
        observations: Arc<Mutex<TestObservations>>,
        arguments: Vec<String>,
    ) -> TrustedLuaRuntimeBindings {
        let calls = Arc::new(TestCalls {
            panic_on_project: false,
            project: test_project(),
            observations: Arc::clone(&observations),
            begin_error: None,
            begin_started: None,
            begin_gate: None,
        });
        let location = RpgMakerLocation::value(
            RpgMakerSource::data(crate::rpg_maker::text::StandardDataFile::Items),
            vec![RpgMakerLocationStep::ArrayIndex(1)],
        );
        let scalar_role =
            TextUnitRole::Scalar(ScalarFieldKey::new("description").expect("测试字段应合法"));
        let units = vec![
            TrustedLuaStandardUnit::new(
                0,
                RpgMakerStandardAssetOwner::Builtin,
                TextGroupKind::DatabaseEntry,
                location,
                scalar_role,
                TextUnitContent::Value("药水".to_owned()),
                "{}".to_owned(),
                None,
                TextUnitContent::Value("⟦PH_1⟧".to_owned()),
                vec![TrustedLuaTranslationTerm::new("药水", "Potion")],
                TrustedLuaStandardLinePolicy::Single,
                TrustedLuaStandardUnitStatus::Missing,
                2,
            ),
            TrustedLuaStandardUnit::new(
                1,
                RpgMakerStandardAssetOwner::Rules,
                TextGroupKind::EventDialogue,
                RpgMakerLocation::value(
                    RpgMakerSource::map(1),
                    vec![RpgMakerLocationStep::ObjectKey("list".to_owned())],
                ),
                TextUnitRole::DialogueBody,
                TextUnitContent::Lines(vec!["第一行".to_owned(), "第二行".to_owned()]),
                r#"{"source_speaker":"莉莉"}"#.to_owned(),
                Some(TextUnitContent::Lines(vec!["旧译文".to_owned()])),
                TextUnitContent::Lines(vec!["第一行".to_owned(), "第二行".to_owned()]),
                Vec::new(),
                TrustedLuaStandardLinePolicy::Reflow,
                TrustedLuaStandardUnitStatus::Stale,
                1,
            ),
        ];
        let standard: Arc<dyn TrustedLuaStandardHostCalls> = Arc::new(TestStandardCalls {
            session: Arc::new(TestStandardSession {
                units,
                observations,
            }),
        });
        TrustedLuaRuntimeBindings::project(
            common_bindings(&calls),
            arguments,
            standard,
            Box::new(TestFinalizer {
                finalizations: Arc::new(Mutex::new(Vec::new())),
                completion: None,
            }),
        )
    }

    #[test]
    fn dense_arrays_reject_holes_and_map_keys() {
        let lua = Lua::new();
        let hole: Table = lua.load("return {[1] = 'a', [3] = 'c'}").eval().unwrap();
        assert!(dense_table_values(hole).is_err());
        let map: Table = lua.load("return {name = 'alice'}").eval().unwrap();
        assert!(dense_table_values(map).is_err());
    }

    #[test]
    fn sqlite_parameters_keep_text_blob_and_null_distinct() {
        let lua = Lua::new();
        let values = lua.create_table().unwrap();
        values.raw_set(1, "text").unwrap();
        values
            .raw_set(2, lua.create_userdata(LuaBlob(vec![0, 255])).unwrap())
            .unwrap();
        values
            .raw_set(3, lua.create_userdata(LuaSqliteNull).unwrap())
            .unwrap();
        assert_eq!(
            parse_parameters(Value::Table(values)).unwrap(),
            vec![
                SqliteValue::Text("text".to_owned()),
                SqliteValue::Blob(vec![0, 255]),
                SqliteValue::Null,
            ]
        );
    }

    #[test]
    fn large_sqlite_and_llm_values_are_accepted() {
        let lua = Lua::new();
        let payload = "字".repeat(1024 * 1024);

        let parameters = lua.create_table_with_capacity(1, 0).unwrap();
        parameters.raw_set(1, payload.as_str()).unwrap();
        let (_, parsed) = parse_sql_call(
            Value::String(lua.create_string("q").unwrap()),
            Value::Table(parameters),
        )
        .unwrap();
        assert_eq!(parsed, [SqliteValue::Text(payload.clone())]);

        let message = lua.create_table().unwrap();
        message.set("role", "user").unwrap();
        message.set("content", payload.as_str()).unwrap();
        let messages = lua.create_table_with_capacity(1, 0).unwrap();
        messages.raw_set(1, message).unwrap();
        let parsed = parse_message_array(Value::Table(messages)).unwrap();
        assert_eq!(parsed[0].content(), payload);
    }

    #[test]
    fn llm_response_without_provider_ids_maps_them_to_lua_nil() {
        let lua = Lua::new();
        let response = LlmResponse::new(
            "translated",
            crate::llm::LlmFinishReason::Stop,
            None,
            None,
            None,
        );
        let Value::Table(table) = llm_response_to_lua(&lua, response).unwrap() else {
            panic!("LLM 响应必须映射成 Lua table");
        };
        assert!(matches!(
            table.get::<Value>("request_id").unwrap(),
            Value::Nil
        ));
        assert!(matches!(
            table.get::<Value>("response_id").unwrap(),
            Value::Nil
        ));
    }

    #[test]
    fn vm_errors_keep_source_text_but_only_publish_the_stable_operation() {
        let sentinel = "底层原因".repeat(10_000);
        let error = vm_error(
            "execute_main_program",
            "Lua 失败",
            mlua::Error::runtime(sentinel.clone()),
        );
        let TrustedLua54RuntimeError::Vm { message, .. } = &error else {
            panic!("VM 错误必须保留文本")
        };
        assert!(message.ends_with(&sentinel));

        let public = error.safe_diagnostic_source(
            DiagnosticStage::Translate,
            DiagnosticImpact::ProgressPreserved,
            DiagnosticAction::FixInput,
        );
        let serialized = serde_json::to_string(&public).expect("VM 安全诊断应可序列化");
        assert!(serialized.contains("lua_vm_operation=execute_main_program"));
        assert!(serialized.contains("translate"));
        assert!(!serialized.contains(&sentinel));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn extract_replace_standard_builds_a_valid_unforgeable_snapshot() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());
        let observations = Arc::new(Mutex::new(TestObservations::default()));
        run_extract_program(
            &runtime,
            Arc::clone(&observations),
            r#"
local items = ctx.rpg_maker.open(ctx.rpg_maker.data("Items.json"))
ctx.extract.replace_standard({
  {
    kind = "database_entry",
    location = items:location({1}),
    fields = {
      { name = "zeta", text = items:text({1, "name"}) },
      { name = "alpha", text = items:text({1, "note"}) },
    },
  },
})
"#,
        )
        .await
        .expect("合法 RPG Maker 引用应该建立 Lua 标准快照");

        {
            let observations = observations.lock().expect("测试观察锁不应中毒");
            let [TrustedLuaStandardExtractIntent::Replace(snapshot)] =
                observations.extract_intents.as_slice()
            else {
                panic!("Runtime 应只记录一次 Replace 意图")
            };
            assert_eq!(snapshot.groups().len(), 1);
            let group = &snapshot.groups()[0];
            assert_eq!(group.kind(), TextGroupKind::DatabaseEntry);
            assert_eq!(group.units().len(), 2);
            let unit = &group.units()[0];
            assert!(matches!(
                unit.role(),
                crate::rpg_maker::model::TextUnitRole::Scalar(key) if key.as_str() == "zeta"
            ));
            assert_eq!(
                unit.source_content(),
                &crate::rpg_maker::model::TextUnitContent::Value("药水".to_owned())
            );
            assert!(matches!(
                group.units()[1].role(),
                crate::rpg_maker::model::TextUnitRole::Scalar(key) if key.as_str() == "alpha"
            ));
            assert_eq!(
                group.units()[1].source_content(),
                &crate::rpg_maker::model::TextUnitContent::Value("<Help:恢复 HP>".to_owned())
            );
        }
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn extract_managed_translations_parse_all_shapes_and_lossless_metadata() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());
        let observations = Arc::new(Mutex::new(TestObservations::default()));
        run_extract_program(
            &runtime,
            Arc::clone(&observations),
            r#"
ctx.extract.replace_standard({})
ctx.translations.replace({
  {
    name = "quest_titles",
    instruction = "翻译任务标题；保持简洁。",
    units = {
      {
        key = "single",
        kind = "plugin_parameter",
        shape = "single",
        original = "星港へ",
        context = "任务标题",
        metadata = ctx.json.object({
          z = ctx.json.number("1e999"),
          quest_id = 12,
          nested = ctx.json.object({tag = "main"}),
        }),
      },
      {
        key = "reflow",
        kind = "plugin_parameter",
        shape = "reflow",
        original = "長い説明",
        context = "",
        metadata = ctx.json.array({true, "tag"}),
      },
      {
        key = "lines",
        kind = "map",
        shape = "lines",
        original = {"第一行", ""},
        context = "",
        metadata = "scalar",
      },
      {
        key = "items",
        kind = "choices",
        shape = "items",
        original = {"はい", "いいえ"},
        context = "",
        metadata = ctx.json.NULL,
      },
    },
  },
})
"#,
        )
        .await
        .expect("四种托管 shape 和显式 JSON metadata 应被完整解析");

        {
            let observations = observations.lock().expect("测试观察锁不应中毒");
            assert!(matches!(
                observations.extract_intents.as_slice(),
                [TrustedLuaStandardExtractIntent::Replace(snapshot)] if snapshot.groups().is_empty()
            ));
            let [snapshot] = observations.managed_snapshots.as_slice() else {
                panic!("Runtime 应只记录一次 managed Replace 意图")
            };
            let [collection] = snapshot.collections() else {
                panic!("测试快照应包含一个 collection")
            };
            assert_eq!(collection.name(), "quest_titles");
            assert_eq!(collection.instruction(), "翻译任务标题；保持简洁。");
            assert_eq!(collection.units().len(), 4);
            assert_eq!(
                collection.units()[0].metadata_json(),
                Some(r#"{"nested":{"tag":"main"},"quest_id":12,"z":1e999}"#)
            );
            assert_eq!(
                collection.units()[0].original(),
                &TrustedLuaManagedTranslationContent::scalar("星港へ")
            );
            assert_eq!(
                collection.units()[1].shape(),
                TrustedLuaManagedTranslationShape::Reflow
            );
            assert_eq!(
                collection.units()[1].metadata_json(),
                Some(r#"[true,"tag"]"#)
            );
            assert_eq!(
                collection.units()[2].original(),
                &TrustedLuaManagedTranslationContent::array(vec![
                    "第一行".to_owned(),
                    String::new(),
                ])
            );
            assert_eq!(collection.units()[2].metadata_json(), Some(r#""scalar""#));
            assert_eq!(
                collection.units()[3].shape(),
                TrustedLuaManagedTranslationShape::Items
            );
            assert_eq!(collection.units()[3].metadata_json(), Some("null"));
        }
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn extract_managed_translations_reject_invalid_snapshots_before_host_calls() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());
        let invalid_programs = [
            r#"ctx.translations.replace({{name="a", instruction="", units={}, extra=true}})"#,
            r#"ctx.translations.replace({[1]={name="a",instruction="",units={}},[3]={name="b",instruction="",units={}}})"#,
            r#"ctx.translations.replace({{name="",instruction="",units={}}})"#,
            r#"ctx.translations.replace({
                 {name="a",instruction="",units={
                   {key="x",kind="map",shape="unknown",original="a",context=""}
                 }}
               })"#,
            r#"ctx.translations.replace({
                 {name="a",instruction="",units={
                   {key="x",kind="map",shape="lines",original={},context=""}
                 }}
               })"#,
            r#"ctx.translations.replace({
                 {name="a",instruction="",units={
                   {key="x",kind="map",shape="items",original={"ok","  "},context=""}
                 }}
               })"#,
            r#"ctx.translations.replace({
                 {name="a",instruction="",units={
                   {key="x",kind="map",shape="single",original="a",context="",metadata={value=1}}
                 }}
               })"#,
            r#"ctx.translations.replace({
                 {name="a",instruction="",units={
                   {key="x",kind="map",shape="single",original="a",context=""},
                   {key="x",kind="map",shape="single",original="b",context=""}
                 }}
               })"#,
            r#"ctx.translations.replace({
                 {name="a",instruction="",units={}},
                 {name="a",instruction="",units={}}
               })"#,
        ];

        for (index, source) in invalid_programs.into_iter().enumerate() {
            let observations = Arc::new(Mutex::new(TestObservations::default()));
            let error = run_extract_program(&runtime, Arc::clone(&observations), source)
                .await
                .unwrap_err();
            assert!(
                matches!(
                    error,
                    TrustedLuaRuntimeExecutionError::Binding(ref error)
                        if error.domain() == "translations"
                            && error.kind() == "invalid_snapshot"
                            && error.operation() == Some("translations.replace")
                ),
                "无效 managed fixture {index} 的实际错误：{error}"
            );
            assert!(
                observations
                    .lock()
                    .expect("测试观察锁不应中毒")
                    .managed_snapshots
                    .is_empty(),
                "无效 managed fixture {index} 不得到达 Host"
            );
        }

        let observations = Arc::new(Mutex::new(TestObservations::default()));
        run_extract_program(
            &runtime,
            Arc::clone(&observations),
            r#"
ctx.translations.replace({})
local ok, error = pcall(ctx.translations.replace, {})
assert(not ok)
assert(error.domain == "translations")
assert(error.kind == "intent_already_declared")
assert(error.operation == "translations.replace")
"#,
        )
        .await
        .expect("第二次 replace 应作为可捕获 Host 错误，而非再次调用 Host");
        assert_eq!(
            observations
                .lock()
                .expect("测试观察锁不应中毒")
                .managed_snapshots
                .len(),
            1
        );

        let observations = Arc::new(Mutex::new(TestObservations::default()));
        run_extract_program(
            &runtime,
            Arc::clone(&observations),
            r#"
local first_ok, first_error = pcall(ctx.translations.replace, 42)
assert(not first_ok)
assert(first_error.operation == "translations.replace")
local second_ok, second_error = pcall(ctx.translations.replace, {})
assert(not second_ok)
assert(second_error.kind == "intent_already_declared")
"#,
        )
        .await
        .expect("一次非法 replace 也必须消耗本脚本唯一调用权");
        assert!(
            observations
                .lock()
                .expect("测试观察锁不应中毒")
                .managed_snapshots
                .is_empty()
        );
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn translate_invalid_first_call_still_consumes_the_only_managed_call() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());
        let observations = Arc::new(Mutex::new(TestObservations::default()));
        let finalizations = Arc::new(Mutex::new(Vec::new()));
        let report = runtime
            .start(
                OwnedLuaProgram::new(
                    PathBuf::from("C:/scripts/translate-managed-once.lua"),
                    br#"
local first_ok, first_error = pcall(ctx.translations.translate, "unexpected")
assert(not first_ok)
assert(first_error.operation == "translations.translate")
local second_ok, second_error = pcall(ctx.translations.translate)
assert(not second_ok)
assert(second_error.kind == "already_translated")
"#
                    .to_vec(),
                ),
                test_bindings(
                    None,
                    Arc::clone(&observations),
                    Arc::clone(&finalizations),
                    None,
                ),
            )
            .await;
        let (execution, finalization) = report.into_parts();
        execution.expect("非法首调后的第二次调用应以可捕获错误结束");
        finalization.expect("测试 finalizer 应成功");
        assert_eq!(
            observations
                .lock()
                .expect("测试观察锁不应中毒")
                .managed_translate_calls,
            0,
            "两次调用都不得到达 Managed Host"
        );
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn extract_replace_standard_rejects_duplicate_group_location_across_kinds() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());
        let observations = Arc::new(Mutex::new(TestObservations::default()));
        let error = run_extract_program(
            &runtime,
            Arc::clone(&observations),
            r#"
local items = ctx.rpg_maker.open(ctx.rpg_maker.data("Items.json"))
local location = items:location({1})
ctx.extract.replace_standard({
  {
    kind = "event_command",
    location = location,
    fields = {{ name = "name", text = items:text({1, "name"}) }},
  },
  {
    kind = "database_entry",
    location = location,
    fields = {{ name = "note", text = items:text({1, "note"}) }},
  },
})
"#,
        )
        .await
        .expect_err("Lua 边界不得把重复 group.location 静默合并");

        let TrustedLuaRuntimeExecutionError::Binding(binding) = &error else {
            panic!("重复组必须映射成普通 Host 参数错误，实际为 {error}")
        };
        assert_eq!(binding.domain(), "extract");
        assert_eq!(binding.kind(), "invalid_standard_snapshot");
        assert!(binding.to_string().contains("group.location"));
        assert!(
            observations
                .lock()
                .expect("测试观察锁不应中毒")
                .extract_intents
                .is_empty(),
            "无效快照不得声明 Replace 意图"
        );
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn extract_replace_standard_accepts_only_single_value_group_kinds() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());

        for kind in ["dialogue", "choices", "scrolling_text"] {
            let observations = Arc::new(Mutex::new(TestObservations::default()));
            let program = format!(
                r#"
local items = ctx.rpg_maker.open(ctx.rpg_maker.data("Items.json"))
ctx.extract.replace_standard({{
  {{
    kind = "{kind}",
    location = items:location({{1}}),
    fields = {{{{ name = "name", text = items:text({{1, "name"}}) }}}},
  }},
}})
"#
            );
            let error = run_extract_program(&runtime, Arc::clone(&observations), program.as_str())
                .await
                .expect_err("复合标准组不能通过单值 Lua Extract 契约建立");
            assert!(matches!(
                error,
                TrustedLuaRuntimeExecutionError::Binding(ref error)
                    if error.kind() == "invalid_standard_snapshot"
            ));
            assert!(
                observations
                    .lock()
                    .expect("测试观察锁不应中毒")
                    .extract_intents
                    .is_empty()
            );
        }

        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn extract_empty_replace_and_clear_have_distinct_intents() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());
        let replace_observations = Arc::new(Mutex::new(TestObservations::default()));
        run_extract_program(
            &runtime,
            Arc::clone(&replace_observations),
            "ctx.extract.replace_standard({})",
        )
        .await
        .expect("空快照应表示 active Lua owner");
        assert!(matches!(
            replace_observations
                .lock()
                .expect("测试观察锁不应中毒")
                .extract_intents
                .as_slice(),
            [TrustedLuaStandardExtractIntent::Replace(snapshot)] if snapshot.groups().is_empty()
        ));

        let clear_observations = Arc::new(Mutex::new(TestObservations::default()));
        run_extract_program(
            &runtime,
            Arc::clone(&clear_observations),
            "ctx.extract.clear_standard()",
        )
        .await
        .expect("clear_standard 应表示停用 Lua owner");
        assert_eq!(
            clear_observations
                .lock()
                .expect("测试观察锁不应中毒")
                .extract_intents
                .as_slice(),
            [TrustedLuaStandardExtractIntent::Deactivate]
        );
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn extract_rejects_duplicate_intents_and_forged_text_references() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());
        let duplicate_observations = Arc::new(Mutex::new(TestObservations::default()));
        let duplicate = run_extract_program(
            &runtime,
            Arc::clone(&duplicate_observations),
            "ctx.extract.clear_standard(); ctx.extract.clear_standard()",
        )
        .await
        .expect_err("一次脚本不得声明两个托管快照意图");
        assert!(matches!(
            duplicate,
            TrustedLuaRuntimeExecutionError::Binding(ref error)
                if error.kind() == "intent_already_declared"
        ));
        assert_eq!(
            duplicate_observations
                .lock()
                .expect("测试观察锁不应中毒")
                .extract_intents
                .as_slice(),
            [TrustedLuaStandardExtractIntent::Deactivate]
        );

        let forged_observations = Arc::new(Mutex::new(TestObservations::default()));
        let forged = run_extract_program(
            &runtime,
            Arc::clone(&forged_observations),
            r#"
local items = ctx.rpg_maker.open(ctx.rpg_maker.data("Items.json"))
ctx.extract.replace_standard({{
  kind = "database_entry",
  location = items:location({1}),
  fields = {{name = "name", text = {original = "伪造"}}},
}})
"#,
        )
        .await
        .expect_err("普通 Lua table 不得伪造 RPG Maker TextRef");
        assert!(matches!(
            forged,
            TrustedLuaRuntimeExecutionError::Binding(ref error)
                if error.kind() == "invalid_standard_snapshot"
        ));
        assert!(
            forged_observations
                .lock()
                .expect("测试观察锁不应中毒")
                .extract_intents
                .is_empty()
        );
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_vm_exposes_exact_ctx_and_preserves_sqlite_and_llm_values() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());
        let observations = Arc::new(Mutex::new(TestObservations::default()));
        let finalizations = Arc::new(Mutex::new(Vec::new()));
        let script = r#"
assert(ctx.phase == "translate")
assert(ctx.project.name == "demo")
assert(ctx.project.engine == "mz")
assert(ctx.project.source_root == [[C:\projects\demo\source]])
assert(ctx.project.database_path == [[C:\projects\demo\project.db]])
assert(ctx.project.source_language == "ja")
assert(ctx.project.target_language == "zh-Hans")
assert(ctx.project.output_root == nil)
assert(ctx.output == nil and ctx.write_back == nil and ctx.extract == nil)
assert(ctx.standard == nil)
assert(type(ctx.translations) == "table")
local open_ok, open_error = pcall(ctx.translations.open, "quest_titles")
assert(not open_ok)
assert(open_error.domain == "translations")
assert(open_error.kind == "translate_required")
local managed_report = ctx.translations.translate()
local managed_results = {}
for result in managed_report:units() do
  managed_results[#managed_results + 1] = result
end
assert(#managed_results == 2)
assert(managed_results[1].collection == "quest_titles")
assert(managed_results[1].key == "single")
assert(managed_results[1].status == "translated")
assert(managed_results[1].translation == "前往星港")
assert(managed_results[1].reason == nil)
assert(ctx.json.kind(managed_results[1].details) == "object")
assert(managed_results[1].details.changed_locations == 2)
assert(managed_results[2].status == "unavailable")
assert(managed_results[2].translation == nil)
assert(managed_results[2].reason == "placeholder_mismatch")
local managed = ctx.translations.open("quest_titles")
assert(managed.name == "quest_titles")
assert(managed.instruction == "翻译任务标题；保持简洁。")
local single = managed:get("single")
assert(single.key == "single" and single.kind == "plugin_parameter")
assert(single.shape == "single" and single.original == "星港へ")
assert(single.context == "任务标题")
assert(single.translation == "前往星港" and single.status == "current")
assert(ctx.json.kind(single.metadata) == "object")
assert(single.metadata.quest_id == 12 and single.metadata.tag == "main")
local managed_units = {}
for unit in managed:units() do managed_units[#managed_units + 1] = unit end
assert(#managed_units == 4)
assert(managed_units[2].shape == "reflow")
assert(managed_units[2].translation == "很长的\n说明")
assert(ctx.json.kind(managed_units[2].metadata) == "array")
assert(#managed_units[2].metadata == 2 and managed_units[2].metadata[2] == "tag")
assert(ctx.json.kind(managed_units[3].original) == "array")
assert(#managed_units[3].original == 2 and managed_units[3].original[2] == "")
assert(managed_units[3].metadata == "tag")
assert(ctx.json.kind(managed_units[4].original) == "array")
assert(managed_units[4].translation == nil and managed_units[4].status == "unavailable")
assert(ctx.json.kind(managed_units[4].metadata) == "null")
assert(ctx.translations.open("missing") == nil)
local mutate_ok = pcall(function() single.translation = "伪造" end)
assert(not mutate_ok)
local twice_ok, twice_error = pcall(ctx.translations.translate)
assert(not twice_ok)
assert(twice_error.domain == "translations" and twice_error.kind == "already_translated")
assert(ctx.translation.system_prompt == "只输出译文")
assert(ctx.translation.language_pair.source == "ja")
assert(ctx.translation.language_pair.target == "zh-Hans")
local kinds = {
  "database_entry",
  "system",
  "map",
  "dialogue",
  "choices",
  "scrolling_text",
  "event_command",
  "plugin_parameter",
}
local prepared
for index, kind in ipairs(kinds) do
  local current = ctx.translation.prepare(kind, "kind:" .. kind, "context:" .. kind)
  assert(current.status == "active")
  if index == 1 then prepared = current end
end
assert(prepared.status == "active")
assert(prepared.model_text == "⟦ATT_COLOR_WHOLE_0000⟧勇者")
assert(#prepared.terms == 2)
assert(prepared.terms[1].term == "勇者")
assert(prepared.terms[1].translation == "Hero")
assert(prepared.terms[2].term == "魔王")
assert(prepared.terms[2].translation == "Demon King")
local non_source = ctx.translation.prepare("map", "__non_source__", "")
assert(non_source.status == "non_source_language")
assert(non_source.model_text == "")
assert(#non_source.terms == 0)
local fully_protected = ctx.translation.prepare("map", "__fully_protected__", "")
assert(fully_protected.status == "fully_protected")
assert(fully_protected.model_text == "")
assert(#fully_protected.terms == 0)
local prepare_ok, prepare_error = pcall(
  ctx.translation.prepare,
  "map",
  "__prepare_error__",
  ""
)
assert(not prepare_ok)
assert(prepare_error.domain == "translation")
assert(prepare_error.kind == "prepare_failed")
local accepted = prepared:accept("勇者")
assert(accepted.accepted == true)
assert(accepted.translation == [=[勇者\C[2]]=])
assert(type(accepted.state) == "string" and #accepted.state == 64)
assert(accepted.state:match("^[0-9a-f]+$") ~= nil)
assert(prepared:is_current(accepted.translation, accepted.state))
assert(not prepared:is_current("其他译文", accepted.state))
local state_ok, state_error = pcall(prepared.is_current, prepared, accepted.translation, string.upper(accepted.state))
assert(not state_ok)
assert(state_error.domain == "translation" and state_error.kind == "invalid_state")
local rejected = prepared:accept("bad")
assert(rejected.accepted == false)
assert(rejected.reason == "source_language_residual")
local accept_ok, accept_error = pcall(prepared.accept, prepared, "__accept_error__")
assert(not accept_ok)
assert(accept_error.domain == "translation")
assert(accept_error.kind == "accept_failed")
assert(type(io.open) == "function")
assert(type(os.execute) == "function")
assert(type(debug.getinfo) == "function")
package.preload["att_test_module"] = function() return {value = 42} end
assert(require("att_test_module").value == 42)

ctx.db.begin()
local rows = ctx.db.query("SELECT values", {
  ctx.db.NULL,
  ctx.db.blob(string.char(0, 255)),
  "input",
  9,
  2.5,
})
assert(#rows == 1 and #rows[1] == 5)
assert(rows[1][1] == ctx.db.NULL)
assert(rows[1][2] == 7)
assert(rows[1][3] == 1.5)
assert(rows[1][4] == "文本")
assert(rows[1][5]:bytes() == string.char(0, 255))
assert(ctx.db.execute("INSERT values", rows[1]) == 1)
local response = ctx.llm({{role = "user", content = "hello"}})
assert(response.content == "raw response")
assert(response.finish_reason == "stop")
assert(response.request_id == "request-1")
assert(response.response_id == "response-1")
assert(response.usage.prompt_tokens == 3)
assert(response.usage.completion_tokens == 5)
assert(response.usage.total_tokens == 8)
ctx.db.commit()
"#;
        let report = runtime
            .start(
                OwnedLuaProgram::new(
                    PathBuf::from("C:/scripts/main.lua"),
                    script.as_bytes().to_vec(),
                ),
                test_bindings(
                    None,
                    Arc::clone(&observations),
                    Arc::clone(&finalizations),
                    None,
                ),
            )
            .await;
        let (execution, finalization) = report.into_parts();
        execution.expect("真实 Lua VM 应执行成功");
        finalization.expect("唯一终结器应成功");
        assert_eq!(
            observations.lock().unwrap().executed_parameters,
            vec![
                SqliteValue::Null,
                SqliteValue::Integer(7),
                SqliteValue::Real(1.5),
                SqliteValue::Text("文本".to_owned()),
                SqliteValue::Blob(vec![0, 255]),
            ]
        );
        {
            let observation_guard = observations.lock().unwrap();
            assert_eq!(
                observation_guard.prepared,
                vec![
                    (
                        TextGroupKind::DatabaseEntry,
                        "kind:database_entry".to_owned(),
                        "context:database_entry".to_owned(),
                    ),
                    (
                        TextGroupKind::System,
                        "kind:system".to_owned(),
                        "context:system".to_owned(),
                    ),
                    (
                        TextGroupKind::Map,
                        "kind:map".to_owned(),
                        "context:map".to_owned(),
                    ),
                    (
                        TextGroupKind::EventDialogue,
                        "kind:dialogue".to_owned(),
                        "context:dialogue".to_owned(),
                    ),
                    (
                        TextGroupKind::EventChoices,
                        "kind:choices".to_owned(),
                        "context:choices".to_owned(),
                    ),
                    (
                        TextGroupKind::EventScrollingText,
                        "kind:scrolling_text".to_owned(),
                        "context:scrolling_text".to_owned(),
                    ),
                    (
                        TextGroupKind::EventCommand,
                        "kind:event_command".to_owned(),
                        "context:event_command".to_owned(),
                    ),
                    (
                        TextGroupKind::PluginParameter,
                        "kind:plugin_parameter".to_owned(),
                        "context:plugin_parameter".to_owned(),
                    ),
                    (
                        TextGroupKind::Map,
                        "__non_source__".to_owned(),
                        String::new()
                    ),
                    (
                        TextGroupKind::Map,
                        "__fully_protected__".to_owned(),
                        String::new(),
                    ),
                    (
                        TextGroupKind::Map,
                        "__prepare_error__".to_owned(),
                        String::new(),
                    ),
                ]
            );
            let messages = &observation_guard.messages;
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].role(), ChatMessageRole::User);
            assert_eq!(messages[0].content(), "hello");
            assert_eq!(observation_guard.managed_translate_calls, 1);
            assert_eq!(
                observation_guard.managed_open_names,
                ["quest_titles".to_owned(), "missing".to_owned()]
            );
        }
        assert_eq!(*finalizations.lock().unwrap(), vec![()]);
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn json_context_preserves_kinds_numbers_and_container_identity() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());
        let script = r#"
local json = ctx.json
assert(json.NULL ~= ctx.db.NULL)
assert(json.kind(json.NULL) == "null")
assert(json.kind(ctx.db.NULL) == nil)
assert(json.kind(true) == "boolean")
assert(json.kind("text") == "string")
assert(json.kind(7) == "number")
assert(json.kind({}) == nil)

local array = json.array()
local object = json.object()
assert(json.kind(array) == "array")
assert(json.kind(object) == "object")
assert(json.encode(array) == "[]")
assert(json.encode(object) == "{}")

local value = json.decode([=[{"z":null,"a":[1,1.25,9223372036854775808,"文本"],"escaped":"\uD83D\uDE00"}]=])
assert(json.kind(value) == "object")
assert(json.kind(value.a) == "array")
assert(value.z == json.NULL)
assert(value.a[1] == 1)
assert(type(value.a[2]) == "userdata")
assert(json.number_text(value.a[2]) == "1.25")
assert(type(value.a[3]) == "userdata")
assert(json.number_text(value.a[3]) == "9223372036854775808")
assert(tostring(value.a[3]) == "9223372036854775808")
assert(value.escaped == "😀")
assert(json.encode(value) == [=[{"a":[1,1.25,9223372036854775808,"文本"],"escaped":"😀","z":null}]=])

local exact = json.number("1e999")
assert(type(exact) == "userdata")
assert(json.kind(exact) == "number")
assert(json.number_text(exact) == "1e999")
assert(json.encode(exact) == "1e999")
local integer = json.number("1")
assert(type(integer) == "number" and integer == json.decode("1"))
assert(json.number_text(1.5) == "1.5")
assert(json.encode(json.object({b = 2, a = 1})) == [[{"a":1,"b":2}]])
"#;
        let report = runtime
            .start(
                OwnedLuaProgram::new(
                    PathBuf::from("C:/scripts/json-roundtrip.lua"),
                    script.as_bytes().to_vec(),
                ),
                test_bindings(
                    None,
                    Arc::new(Mutex::new(TestObservations::default())),
                    Arc::new(Mutex::new(Vec::new())),
                    None,
                ),
            )
            .await;
        report.into_parts().0.unwrap();
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn json_context_rejects_ambiguous_or_non_json_lua_values() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());
        let script = r#"
local json = ctx.json
local failures = {
  function() return json.decode([[{"a":1,"a":2}]]) end,
  function() return json.number("01") end,
  function() return json.encode({1, 2}) end,
  function() return json.encode(json.array({[1] = 1, [3] = 3})) end,
  function() return json.encode(json.array({[1] = 1, name = 2})) end,
  function() return json.encode(json.object({[1] = 1})) end,
  function()
    local value = json.array()
    value[1] = value
    return json.encode(value)
  end,
  function() return json.encode(0 / 0) end,
  function() return json.encode(math.huge) end,
  function() return json.number_text(math.huge) end,
  function() return json.encode(ctx.db.blob("bytes")) end,
  function() return json.encode(ctx.db.NULL) end,
  function() return json.array(json.object()) end,
}
for _, failure in ipairs(failures) do
  local ok, error = pcall(failure)
  assert(not ok)
  assert(type(error) == "userdata")
  assert(error.domain == "json")
  assert(error.kind == "invalid_value")
  assert(type(error.message) == "string" and #error.message > 0)
  assert(error.retry_after_ms == nil)
end
local ok, error = pcall(ctx.db.query, "SELECT values", {ctx.json.NULL})
assert(not ok and error.domain == "binding" and error.kind == "invalid_value")
"#;
        let report = runtime
            .start(
                OwnedLuaProgram::new(
                    PathBuf::from("C:/scripts/json-invalid.lua"),
                    script.as_bytes().to_vec(),
                ),
                test_bindings(
                    None,
                    Arc::new(Mutex::new(TestObservations::default())),
                    Arc::new(Mutex::new(Vec::new())),
                    None,
                ),
            )
            .await;
        report.into_parts().0.unwrap();
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn json_context_handles_deep_and_large_values() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());
        let script = r#"
local json = ctx.json
assert(json.encode(json.decode("[1,2]")) == "[1,2]")
local depth = 10000
local source = string.rep("[", depth) .. "0" .. string.rep("]", depth)
assert(json.encode(json.decode(source)) == source)
local large = string.rep("值", 1024 * 1024)
assert(json.decode(json.encode(large)) == large)
"#;
        let report = runtime
            .start(
                OwnedLuaProgram::new(
                    PathBuf::from("C:/scripts/json-unbounded.lua"),
                    script.as_bytes().to_vec(),
                ),
                test_bindings(
                    None,
                    Arc::new(Mutex::new(TestObservations::default())),
                    Arc::new(Mutex::new(Vec::new())),
                    None,
                ),
            )
            .await;
        report.into_parts().0.unwrap();
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn json_markers_do_not_pin_discarded_containers_against_gc() {
        // markers 表以弱键登记 JSON 容器:脚本丢弃引用后 GC 必须能回收容器,
        // 逐文档处理的 VM 峰值内存不随已解码文档数单调累积;
        // 仍被引用的容器保持标记语义(encode round-trip 不受 GC 影响)。
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());
        let script = r#"
local json = ctx.json
collectgarbage("collect")
collectgarbage("collect")
local baseline = collectgarbage("count")
for _ = 1, 64 do
    local document = json.decode('{"items":[' .. string.rep('{"v":"数据"},', 511) .. '{"v":"数据"}]}')
    assert(document.items[1].v == "数据")
end
collectgarbage("collect")
collectgarbage("collect")
local grown = collectgarbage("count") - baseline
-- 64 份文档各含 512 个对象;若 markers 钉住全部容器,增长在数十 MiB 量级。
assert(grown < 4096, "丢弃的 JSON 容器必须可被 GC 回收，泄漏 KiB=" .. tostring(grown))

local kept = json.decode('[{"v":1}]')
collectgarbage("collect")
assert(json.encode(kept) == '[{"v":1}]', "仍被引用的容器必须保留数组/对象标记")
"#;
        let report = runtime
            .start(
                OwnedLuaProgram::new(
                    PathBuf::from("C:/scripts/json-markers-gc.lua"),
                    script.as_bytes().to_vec(),
                ),
                test_bindings(
                    None,
                    Arc::new(Mutex::new(TestObservations::default())),
                    Arc::new(Mutex::new(Vec::new())),
                    None,
                ),
            )
            .await;
        report.into_parts().0.unwrap();
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn source_and_rpg_maker_facades_reuse_lossless_json_and_structured_locations() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());
        let script = r#"
assert(ctx.source.read("js/raw.bin") == string.char(0, 255, 1))
assert(ctx.source.read_text("js/text.txt") == "文本")
local raw_json = ctx.source.read_json("js/value.json")
assert(ctx.json.kind(raw_json) == "object")
assert(ctx.json.kind(raw_json.array) == "array")
assert(ctx.json.number_text(raw_json.large) == "1e999")
local files = ctx.source.list("data")
assert(ctx.json.kind(files) == "array")
assert(files[1] == "data/Items.json" and files[2] == "data/Map001.json")

for _, path in ipairs({"../data/Items.json", "C:/game/data/Items.json", "data/file:stream", "other/file"}) do
  local ok, error = pcall(ctx.source.read, path)
  assert(not ok)
  assert(error.domain == "binding")
  assert(error.kind == "invalid_source_path")
end

local item_source = ctx.rpg_maker.data("Items.json")
assert(type(item_source) == "userdata")
assert(tostring(item_source) == "data/Items.json")
assert(tostring(ctx.rpg_maker.data_file("Items.json")) == "data/Items.json")
assert(tostring(ctx.rpg_maker.data_file("Map001.json")) == "data/Map001.json")
assert(tostring(ctx.rpg_maker.data_file("Map000.json")) == "data/Map000.json")
assert(tostring(ctx.rpg_maker.data_file("Custom.json")) == "data/Custom.json")
local items = ctx.rpg_maker.open(item_source)
assert(type(items) == "userdata")
local value = items:value({1, "nested", ctx.rpg_maker.DECODE_JSON, 0, "Title"})
assert(value == "任务")
local location = items:location({1, "name"})
assert(type(location) == "userdata")
assert(tostring(location) == "data/Items.json[1].name")
local text = items:text({1, "name"})
assert(type(text) == "userdata")
assert(text.original == "药水")
assert(type(text.location) == "userdata")
assert(tostring(text.location) == "data/Items.json[1].name")

local plugin = ctx.rpg_maker.open(ctx.rpg_maker.plugin_parameter(0, "Quest", "Entries"))
local plugin_text = plugin:text({ctx.rpg_maker.DECODE_JSON, 0, "Title"})
assert(plugin_text.original == "插件任务")
assert(tostring(plugin_text.location) == "plugins.js[Quest].Entries<json>[0].Title")

local ok, error = pcall(ctx.rpg_maker.data, "Custom.json")
assert(not ok and error.domain == "rpg_maker" and error.kind == "invalid_source")
ok, error = pcall(ctx.rpg_maker.data_file, "../Custom.json")
assert(not ok and error.domain == "rpg_maker" and error.kind == "invalid_source")
ok, error = pcall(ctx.rpg_maker.map, 0)
assert(not ok and error.domain == "rpg_maker" and error.kind == "invalid_source")
ok, error = pcall(ctx.rpg_maker.plugin_parameter, -1, "Quest", "Entries")
assert(not ok and error.domain == "rpg_maker" and error.kind == "invalid_plugin_parameter_source")
ok, error = pcall(ctx.rpg_maker.open, {})
assert(not ok and error.domain == "rpg_maker" and error.kind == "invalid_argument")
ok, error = pcall(items.value, items, {1, -1})
assert(not ok and error.domain == "rpg_maker" and error.kind == "invalid_argument")
ok, error = pcall(items.text, items, {1, "missing"})
assert(not ok and error.domain == "rpg_maker" and error.kind == "invalid_location")
"#;
        let report = runtime
            .start(
                OwnedLuaProgram::new(
                    PathBuf::from("C:/scripts/source-rpg-maker.lua"),
                    script.as_bytes().to_vec(),
                ),
                test_bindings(
                    None,
                    Arc::new(Mutex::new(TestObservations::default())),
                    Arc::new(Mutex::new(Vec::new())),
                    None,
                ),
            )
            .await;
        report.into_parts().0.unwrap();
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn project_lua_exposes_arguments_and_standard_session_without_phase_capabilities() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());
        let observations = Arc::new(Mutex::new(TestObservations::default()));
        let script = r#"
assert(ctx.phase == "lua")
assert(ctx.extract == nil and ctx.translation == nil and ctx.llm == nil)
assert(ctx.output == nil and ctx.write_back == nil)
assert(ctx.translations == nil)
assert(type(ctx.project) == "table")
assert(type(ctx.json) == "table")
assert(type(ctx.source) == "table")
assert(type(ctx.rpg_maker) == "table")
assert(type(ctx.db) == "table")
assert(type(ctx.standard) == "table")
assert(arg[0] == "C:/scripts/project.lua")
assert(arg[1] == "第一" and arg[2] == "--literal")

local standard = ctx.standard.open()
local units = {}
for unit in standard:units() do
  units[#units + 1] = unit
end
assert(#units == 2)

local scalar = units[1]
assert(scalar.owner == "builtin")
assert(scalar.group_kind == "database_entry")
assert(tostring(scalar.group_location) == "data/Items.json[1]")
assert(scalar.role.kind == "scalar" and scalar.role.field == "description")
assert(scalar.original == "药水")
assert(ctx.json.kind(scalar.source_context) == "object")
assert(scalar.translation == nil)
assert(scalar.model_text == "⟦PH_1⟧")
assert(ctx.json.kind(scalar.terms) == "array")
assert(scalar.terms[1].term == "药水" and scalar.terms[1].translation == "Potion")
assert(scalar.content_kind == "value")
assert(scalar.line_policy == "single" and scalar.expected_line_count == 1)
assert(scalar.status == "missing" and scalar.family_size == 2)

local body = units[2]
assert(body.content_kind == "lines")
assert(body.line_policy == "reflow" and body.expected_line_count == nil)
assert(body.status == "stale")
assert(#body.original == 2 and body.original[2] == "第二行")
assert(body.translation[1] == "旧译文")
assert(body.source_context.source_speaker == "莉莉")

local items = ctx.rpg_maker.open(ctx.rpg_maker.data("Items.json"))
local selected = standard:get("builtin", items:location({1}), {
  kind = "scalar",
  field = "description",
})
assert(selected ~= nil and selected.original == "药水")
assert(standard:get("rules", items:location({1}), {kind = "dialogue_body"}) == nil)

local results = standard:accept({
  {unit = scalar, candidate = "人工译文", replace_current = false},
  {unit = body, candidate = {"译文一", "译文二"}, replace_current = true},
})
assert(#results == 2)
assert(results[1].accepted == true)
assert(results[1].translation == "人工译文")
assert(results[1].changed_locations == 2)
assert(results[2].accepted == false)
assert(results[2].reason == "source_residual")
assert(results[2].line == 2)

local another = ctx.standard.open()
local ok, error = pcall(another.accept, another, {
  {unit = scalar, candidate = "不能跨会话"},
})
assert(not ok)
assert(error.domain == "standard" and error.kind == "foreign_unit")

ok, error = pcall(standard.accept, standard, {
  {unit = scalar, candidate = {"不是标量"}},
})
assert(not ok and error.domain == "standard" and error.kind == "invalid_argument")
ok, error = pcall(standard.accept, standard, {
  {unit = body, candidate = "不是 Lines"},
})
assert(not ok and error.domain == "standard" and error.kind == "invalid_argument")
ok, error = pcall(standard.accept, standard, {
  {unit = body, candidate = {[1] = "第一行", [3] = "第三行"}},
})
assert(not ok and error.domain == "standard" and error.kind == "invalid_argument")

return "ignored"
"#;
        let report = runtime
            .start(
                OwnedLuaProgram::new(
                    PathBuf::from("C:/scripts/project.lua"),
                    script.as_bytes().to_vec(),
                ),
                project_bindings(
                    Arc::clone(&observations),
                    vec!["第一".to_owned(), "--literal".to_owned()],
                ),
            )
            .await;
        let (execution, finalization) = report.into_parts();
        execution.expect("独立项目 Lua 应执行成功");
        finalization.expect("独立项目 Lua 应完成唯一终结");
        {
            let observation_guard = observations.lock().expect("测试观察锁不应中毒");
            let candidates = &observation_guard.standard_candidates;
            assert_eq!(candidates.len(), 2);
            assert_eq!(
                candidates[0].candidate(),
                &TextUnitContent::Value("人工译文".to_owned())
            );
            assert!(!candidates[0].replace_current());
            assert_eq!(
                candidates[1].candidate(),
                &TextUnitContent::Lines(vec!["译文一".to_owned(), "译文二".to_owned()])
            );
            assert!(candidates[1].replace_current());
        }
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn empty_project_lua_program_succeeds() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());
        let report = runtime
            .start(
                OwnedLuaProgram::new(PathBuf::from("C:/scripts/empty.lua"), Vec::new()),
                project_bindings(
                    Arc::new(Mutex::new(TestObservations::default())),
                    Vec::new(),
                ),
            )
            .await;
        let (execution, finalization) = report.into_parts();
        execution.expect("零字节 Lua 是合法空程序");
        finalization.expect("零字节 Lua 也必须完成唯一终结");
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pcall_receives_typed_host_error_and_unhandled_error_is_binding_failure() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());
        let host_error =
            TrustedLuaHostCallError::new("sqlite", "busy", "database busy", Some(25), None);
        let caught = runtime
            .start(
                OwnedLuaProgram::new(
                    PathBuf::from("C:/scripts/caught.lua"),
                    br#"
local ok, error = pcall(ctx.db.begin)
assert(not ok)
assert(error.domain == "sqlite")
assert(error.kind == "busy")
assert(error.message == "database busy")
assert(error.retry_after_ms == 25)
"#
                    .to_vec(),
                ),
                test_bindings(
                    Some(host_error.clone()),
                    Arc::new(Mutex::new(TestObservations::default())),
                    Arc::new(Mutex::new(Vec::new())),
                    None,
                ),
            )
            .await;
        assert!(caught.into_parts().0.is_ok());

        let unhandled = runtime
            .start(
                OwnedLuaProgram::new(
                    PathBuf::from("C:/scripts/unhandled.lua"),
                    b"ctx.db.begin()".to_vec(),
                ),
                test_bindings(
                    Some(host_error),
                    Arc::new(Mutex::new(TestObservations::default())),
                    Arc::new(Mutex::new(Vec::new())),
                    None,
                ),
            )
            .await;
        assert!(matches!(
            unhandled.into_parts().0,
            Err(TrustedLuaRuntimeExecutionError::Binding(error))
                if error.domain() == "sqlite" && error.kind() == "busy"
        ));

        let unhandled_json = runtime
            .start(
                OwnedLuaProgram::new(
                    PathBuf::from("C:/scripts/unhandled-json.lua"),
                    br#"ctx.json.decode("{")"#.to_vec(),
                ),
                test_bindings(
                    None,
                    Arc::new(Mutex::new(TestObservations::default())),
                    Arc::new(Mutex::new(Vec::new())),
                    None,
                ),
            )
            .await;
        assert!(matches!(
            unhandled_json.into_parts().0,
            Err(TrustedLuaRuntimeExecutionError::Binding(error))
                if error.domain() == "json" && error.kind() == "invalid_value"
        ));

        let invalid_binding = runtime
            .start(
                OwnedLuaProgram::new(
                    PathBuf::from("C:/scripts/invalid-binding.lua"),
                    br#"
local ok, error = pcall(ctx.db.query, "SELECT values", {[1] = 1, [3] = 3})
assert(not ok)
assert(error.domain == "binding")
assert(error.kind == "invalid_value")

ok, error = pcall(ctx.llm, {{role = "user", content = "hello", extra = true}})
assert(not ok)
assert(error.domain == "binding")
assert(error.kind == "invalid_value")

ok, error = pcall(ctx.translation.prepare, "unknown", "text")
assert(not ok)
assert(error.domain == "binding")
assert(error.kind == "invalid_value")

ok, error = pcall(ctx.translation.prepare, "map", "text")
assert(not ok)
assert(error.domain == "binding")
assert(error.kind == "invalid_value")

local prepared = ctx.translation.prepare("map", "text", "")
ok, error = pcall(prepared.accept, prepared, 42)
assert(not ok)
assert(error.domain == "binding")
assert(error.kind == "invalid_value")
"#
                    .to_vec(),
                ),
                test_bindings(
                    None,
                    Arc::new(Mutex::new(TestObservations::default())),
                    Arc::new(Mutex::new(Vec::new())),
                    None,
                ),
            )
            .await;
        invalid_binding.into_parts().0.unwrap();
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_unpolled_handle_cancels_but_supervisor_still_finalizes_once() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());
        let (completion, finalized) = oneshot::channel();
        let handle = runtime.start(
            OwnedLuaProgram::new(
                PathBuf::from("C:/scripts/infinite.lua"),
                b"while true do end".to_vec(),
            ),
            test_bindings(
                None,
                Arc::new(Mutex::new(TestObservations::default())),
                Arc::new(Mutex::new(Vec::new())),
                Some(completion),
            ),
        );
        drop(handle);
        tokio::time::timeout(Duration::from_secs(5), finalized)
            .await
            .expect("取消后应完成唯一终结")
            .expect("终结器应发送终态");
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_the_last_runtime_handle_requests_cancellation_and_finalization() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());
        let (completion, finalized) = oneshot::channel();
        let execution = runtime.start(
            OwnedLuaProgram::new(
                PathBuf::from("C:/scripts/drop-runtime.lua"),
                b"while true do end".to_vec(),
            ),
            test_bindings(
                None,
                Arc::new(Mutex::new(TestObservations::default())),
                Arc::new(Mutex::new(Vec::new())),
                Some(completion),
            ),
        );

        drop(runtime);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), finalized)
                .await
                .expect("最后一个 Runtime 句柄释放后应完成终结")
                .expect("终结器应返回终态"),
            ()
        );
        let (runtime, finalization) = execution.await.into_parts();
        assert!(
            matches!(&runtime, Err(TrustedLuaRuntimeExecutionError::Cancelled)),
            "最后一个 Runtime 句柄释放后的实际执行终态：{runtime:?}"
        );
        finalization.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_interrupts_a_host_await_and_then_finalizes() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());
        let started = Arc::new(Notify::new());
        let gate = Arc::new(Notify::new());
        let (completion, finalized) = oneshot::channel();
        let calls = Arc::new(TestCalls {
            panic_on_project: false,
            project: test_project(),
            observations: Arc::new(Mutex::new(TestObservations::default())),
            begin_error: None,
            begin_started: Some(Arc::clone(&started)),
            begin_gate: Some(gate),
        });
        let bindings = translate_bindings(
            calls,
            Box::new(TestFinalizer {
                finalizations: Arc::new(Mutex::new(Vec::new())),
                completion: Some(completion),
            }),
        );
        let handle = runtime.start(
            OwnedLuaProgram::new(
                PathBuf::from("C:/scripts/host-await.lua"),
                b"ctx.db.begin()".to_vec(),
            ),
            bindings,
        );
        tokio::time::timeout(Duration::from_secs(5), started.notified())
            .await
            .expect("Lua 应已进入 Host await");
        drop(handle);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), finalized)
                .await
                .expect("Host await 取消后应终结")
                .expect("终结器应返回终态"),
            ()
        );
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_waits_for_an_accepted_output_edit_before_finalizing() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());
        let started = Arc::new(Notify::new());
        let gate = Arc::new(Notify::new());
        let observations = Arc::new(Mutex::new(TestObservations::default()));
        let (completion, mut finalized) = oneshot::channel();
        let opened = crate::rpg_maker::project::OpenedProject::new(
            "demo".parse::<ProjectName>().unwrap(),
            PathBuf::from("C:/projects/demo"),
            PathBuf::from("C:/projects/demo/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            crate::rpg_maker::project::test_layout_profile(),
        );
        let calls = Arc::new(TestCalls {
            panic_on_project: false,
            project: LuaProjectContext::for_write_back_candidate(
                opened.name().as_str(),
                opened.layout().rpg_maker_layout().engine(),
                opened.source_root().to_path_buf(),
                opened.database_path().to_path_buf(),
                opened.language_pair().clone(),
                PathBuf::from("C:/projects/demo/.write_back-stage"),
            ),
            observations: Arc::clone(&observations),
            begin_error: None,
            begin_started: Some(Arc::clone(&started)),
            begin_gate: Some(Arc::clone(&gate)),
        });
        let handle = runtime.start(
            OwnedLuaProgram::new(
                PathBuf::from("C:/scripts/gated-write.lua"),
                b"ctx.output.write('data/gated.bin', 'finished')".to_vec(),
            ),
            write_back_bindings(
                calls,
                Box::new(TestFinalizer {
                    finalizations: Arc::new(Mutex::new(Vec::new())),
                    completion: Some(completion),
                }),
            ),
        );
        tokio::time::timeout(Duration::from_secs(5), started.notified())
            .await
            .expect("Lua 应已把候选写入交给根");

        drop(handle);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut finalized)
                .await
                .is_err(),
            "根尚未交还候选写入终态时不得运行 finalizer"
        );

        gate.notify_one();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), finalized)
                .await
                .expect("候选写入终结后应完成 finalization")
                .expect("finalizer 应交还终态"),
            ()
        );
        assert_eq!(
            observations
                .lock()
                .expect("测试观察锁不应中毒")
                .output_writes,
            vec![("data/gated.bin".to_owned(), b"finished".to_vec())]
        );
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_vm_only_exposes_llm_in_translate_and_output_in_write_back() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());
        let finalizations = Arc::new(Mutex::new(Vec::new()));
        let extract_calls = Arc::new(TestCalls {
            panic_on_project: false,
            project: test_project(),
            observations: Arc::new(Mutex::new(TestObservations::default())),
            begin_error: None,
            begin_started: None,
            begin_gate: None,
        });
        let extract = runtime.start(
                OwnedLuaProgram::new(
                    PathBuf::from("C:/scripts/extract.lua"),
                    b"assert(ctx.phase == 'extract'); assert(ctx.llm == nil); assert(ctx.translation == nil); assert(ctx.output == nil); assert(ctx.write_back == nil); assert(ctx.standard == nil); assert(ctx.project.output_root == nil); assert(type(ctx.translations) == 'table'); assert(ctx.translations.translate == nil and ctx.translations.open == nil); assert(type(ctx.source) == 'table'); assert(type(ctx.rpg_maker) == 'table')".to_vec(),
                ),
                extract_bindings(
                    extract_calls,
                    Box::new(TestFinalizer {
                        finalizations: Arc::clone(&finalizations),
                        completion: None,
                    }),
                ),
            )
            .await;
        extract.into_parts().0.unwrap();

        let opened = crate::rpg_maker::project::OpenedProject::new(
            "demo".parse::<ProjectName>().unwrap(),
            PathBuf::from("C:/projects/demo"),
            PathBuf::from("C:/projects/demo/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            crate::rpg_maker::project::test_layout_profile(),
        );
        let write_back_observations = Arc::new(Mutex::new(TestObservations::default()));
        let write_back_calls = Arc::new(TestCalls {
            panic_on_project: false,
            project: LuaProjectContext::for_write_back_candidate(
                opened.name().as_str(),
                opened.layout().rpg_maker_layout().engine(),
                opened.source_root().to_path_buf(),
                opened.database_path().to_path_buf(),
                opened.language_pair().clone(),
                PathBuf::from("C:/projects/demo/write_back"),
            ),
            observations: Arc::clone(&write_back_observations),
            begin_error: None,
            begin_started: None,
            begin_gate: None,
        });
        let write_back = runtime
            .start(
                OwnedLuaProgram::new(
                    PathBuf::from("C:/scripts/write.lua"),
                    r#"
assert(ctx.phase == 'write_back')
assert(ctx.llm == nil and ctx.translation == nil and ctx.extract == nil)
assert(ctx.standard == nil)
assert(ctx.project.output_root == 'C:/projects/demo/write_back')
assert(type(ctx.source) == 'table' and type(ctx.rpg_maker) == 'table')
assert(type(ctx.output) == 'table' and type(ctx.write_back) == 'table')
assert(type(ctx.translations) == 'table')
assert(ctx.translations.translate == nil and ctx.translations.replace == nil)
local managed = ctx.translations.open('quest_titles')
assert(managed.name == 'quest_titles')
assert(managed:get('single').translation == '前往星港')
assert(ctx.json.kind(managed:get('reflow').metadata) == 'array')
assert(managed:get('lines').metadata == 'tag')
assert(ctx.json.kind(managed:get('items').metadata) == 'null')
assert(ctx.translations.open('missing') == nil)

assert(ctx.output.read('data/raw.bin') == string.char(0, 255, 1))
assert(ctx.output.read_text('data/text.txt') == '文本')
local value = ctx.output.read_json('data/value.json')
assert(ctx.json.kind(value) == 'object')
assert(ctx.json.number_text(value.large) == '1e999')
local entries = ctx.output.list('data')
assert(ctx.json.kind(entries) == 'array' and #entries == 2)
assert(entries[1].name == 'nested' and entries[1].kind == 'directory')
assert(entries[2].name == 'text.txt' and entries[2].kind == 'file')

ctx.output.create_directory('data/generated')
ctx.output.write('data/generated/raw.bin', string.char(0, 255))
ctx.output.write_text('data/generated/text.txt', '写回')
ctx.output.write_json('data/generated/value.json', ctx.json.object({b = 2, a = 1}))
ctx.output.remove('data/generated/text.txt')

local laid_out = ctx.write_back.layout('dialogue_body', {
  { original = '原文', translation = '译文' },
  { original = '冻结原文' },
})
assert(laid_out.status == 'applied')
assert(laid_out.texts[1] == '译文' and laid_out.texts[2] == '冻结原文')
assert(laid_out.inserted_line_breaks == 1)
assert(laid_out.inserted_fullwidth_indents == 2)

local ok, error = pcall(ctx.output.read, '../project.db')
assert(not ok and error.domain == 'binding' and error.kind == 'invalid_output_path')
ok, error = pcall(ctx.write_back.layout, 'unknown', {})
assert(not ok and error.domain == 'binding' and error.kind == 'invalid_value')
"#
                    .as_bytes()
                    .to_vec(),
                ),
                write_back_bindings(
                    write_back_calls,
                    Box::new(TestFinalizer {
                        finalizations: Arc::clone(&finalizations),
                        completion: None,
                    }),
                ),
            )
            .await;
        write_back.into_parts().0.unwrap();
        {
            let write_back_observations =
                write_back_observations.lock().expect("测试观察锁不应中毒");
            assert_eq!(
                write_back_observations.output_operations,
                vec![
                    "create:data/generated".to_owned(),
                    "remove:data/generated/text.txt".to_owned(),
                ]
            );
            assert_eq!(
                write_back_observations.output_writes,
                vec![
                    ("data/generated/raw.bin".to_owned(), vec![0, 255]),
                    (
                        "data/generated/text.txt".to_owned(),
                        "写回".as_bytes().to_vec(),
                    ),
                    (
                        "data/generated/value.json".to_owned(),
                        br#"{"a":1,"b":2}"#.to_vec(),
                    ),
                ]
            );
            assert_eq!(write_back_observations.layouts.len(), 1);
            assert_eq!(
                write_back_observations.layouts[0].0,
                TrustedLuaWriteBackLayoutRegion::DialogueBody
            );
            assert_eq!(
                write_back_observations.layouts[0].1,
                vec![
                    TrustedLuaWriteBackLayoutPair::new("原文".to_owned(), Some("译文".to_owned()),),
                    TrustedLuaWriteBackLayoutPair::new("冻结原文".to_owned(), None),
                ]
            );
            assert_eq!(
                write_back_observations.managed_open_names,
                ["quest_titles".to_owned(), "missing".to_owned()]
            );
        }
        assert_eq!(*finalizations.lock().unwrap(), vec![(), ()]);
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn worker_panic_is_isolated_and_still_runs_the_unique_finalizer() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());
        let finalizations = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(TestCalls {
            panic_on_project: true,
            project: test_project(),
            observations: Arc::new(Mutex::new(TestObservations::default())),
            begin_error: None,
            begin_started: None,
            begin_gate: None,
        });
        let report = runtime
            .start(
                OwnedLuaProgram::new(
                    PathBuf::from("C:/scripts/panic.lua"),
                    b"return true".to_vec(),
                ),
                extract_bindings(
                    calls,
                    Box::new(TestFinalizer {
                        finalizations: Arc::clone(&finalizations),
                        completion: None,
                    }),
                ),
            )
            .await;
        let (execution, finalization) = report.into_parts();
        assert!(matches!(
            execution,
            Err(TrustedLuaRuntimeExecutionError::WorkerPanicked)
        ));
        finalization.unwrap();
        assert_eq!(*finalizations.lock().unwrap(), [()]);
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn synchronous_finalizer_panic_becomes_a_cleanup_report() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());
        let calls = Arc::new(TestCalls {
            panic_on_project: false,
            project: test_project(),
            observations: Arc::new(Mutex::new(TestObservations::default())),
            begin_error: None,
            begin_started: None,
            begin_gate: None,
        });
        let report = runtime
            .start(
                OwnedLuaProgram::new(
                    PathBuf::from("C:/scripts/finalizer-panic.lua"),
                    b"return true".to_vec(),
                ),
                extract_bindings(calls, Box::new(PanickingFinalizer)),
            )
            .await;
        let (execution, finalization) = report.into_parts();
        execution.unwrap();
        assert!(finalization.is_err());
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snapshotted_main_program_survives_deletion_and_requires_from_its_saved_parent() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("local_helper.lua"),
            "return { value = 'loaded' }",
        )
        .unwrap();
        let main_script = directory.path().join("main.lua");
        std::fs::write(
            &main_script,
            "local value, loader = require('local_helper'); assert(value.value == 'loaded'); assert(string.find(loader, '@att-utf16-', 1, true) == 1)",
        )
        .unwrap();
        let snapshot = std::fs::read(&main_script).unwrap();
        std::fs::remove_file(&main_script).unwrap();
        assert!(!main_script.exists());

        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());
        let report = runtime
            .start(
                OwnedLuaProgram::new(main_script, snapshot),
                test_bindings(
                    None,
                    Arc::new(Mutex::new(TestObservations::default())),
                    Arc::new(Mutex::new(Vec::new())),
                    None,
                ),
            )
            .await;
        report.into_parts().0.unwrap();
        runtime.shutdown().await.unwrap();
    }

    #[test]
    fn path_identity_is_ascii_control_free_and_preserves_each_utf16_code_unit() {
        let path = PathBuf::from(OsString::from_wide(&[
            0x0043, 0x003A, 0x005C, 0xD83D, 0xDE00, 0x005C, 0xD800, 0x005C, 0x0009,
        ]));

        let identity = safe_path_identity(&path);

        assert_eq!(
            identity,
            "@att-utf16-0043-003A-005C-D83D-DE00-005C-D800-005C-0009"
        );
        assert!(identity.is_ascii());
        assert!(!identity.chars().any(char::is_control));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn in_memory_main_and_pure_lua_module_support_an_unpaired_surrogate_parent() {
        let directory = tempfile::tempdir().unwrap();
        let script_directory = directory.path().join(OsString::from_wide(&[0xD800]));
        std::fs::create_dir(&script_directory).unwrap();
        std::fs::write(
            script_directory.join("local_helper.lua"),
            "return { value = 'loaded' }",
        )
        .unwrap();
        let main_script = script_directory.join("main.lua");
        let script = br#"
local source = debug.getinfo(1, "S").source
assert(string.find(source, "@att-utf16-", 1, true) == 1)
assert(string.find(source, "D800", 1, true) ~= nil)
assert(string.find(source, "%c") == nil)
assert(#package.searchers == 2)
assert(package.cpath == nil)
assert(package.loadlib == nil)
local value, loader = require("local_helper")
assert(value.value == "loaded")
assert(string.find(loader, "D800", 1, true) ~= nil)
"#;
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());

        let report = runtime
            .start(
                OwnedLuaProgram::new(main_script, script.to_vec()),
                test_bindings(
                    None,
                    Arc::new(Mutex::new(TestObservations::default())),
                    Arc::new(Mutex::new(Vec::new())),
                    None,
                ),
            )
            .await;

        report.into_parts().0.unwrap();
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lua_vm_allows_large_allocation() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());
        let report = runtime
            .start(
                OwnedLuaProgram::new(
                    PathBuf::from("C:/scripts/memory.lua"),
                    b"local value = string.rep('x', 32 * 1024 * 1024); assert(#value > 0)".to_vec(),
                ),
                test_bindings(
                    None,
                    Arc::new(Mutex::new(TestObservations::default())),
                    Arc::new(Mutex::new(Vec::new())),
                    None,
                ),
            )
            .await;
        report.into_parts().0.unwrap();
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_cancels_an_accepted_script_and_waits_for_finalization() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());
        let (completion, mut finalized) = oneshot::channel();
        let execution = runtime.start(
            OwnedLuaProgram::new(
                PathBuf::from("C:/scripts/shutdown.lua"),
                b"while true do end".to_vec(),
            ),
            test_bindings(
                None,
                Arc::new(Mutex::new(TestObservations::default())),
                Arc::new(Mutex::new(Vec::new())),
                Some(completion),
            ),
        );
        let shutdown_runtime = runtime.clone();
        let shutdown = tokio::spawn(async move { shutdown_runtime.shutdown().await });

        shutdown.await.unwrap().unwrap();
        assert_eq!(finalized.try_recv(), Ok(()));
        let (execution, _) = execution.await.into_parts();
        assert!(
            matches!(execution, Err(TrustedLuaRuntimeExecutionError::Cancelled)),
            "shutdown 后的脚本终态应为取消，实际为 {execution:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_after_shutdown_still_finalizes_the_owned_bindings() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());
        runtime.shutdown().await.unwrap();
        let finalizations = Arc::new(Mutex::new(Vec::new()));
        let report = runtime
            .start(
                OwnedLuaProgram::new(
                    PathBuf::from("C:/scripts/rejected.lua"),
                    b"return true".to_vec(),
                ),
                test_bindings(
                    None,
                    Arc::new(Mutex::new(TestObservations::default())),
                    Arc::clone(&finalizations),
                    None,
                ),
            )
            .await;
        let (execution, finalization) = report.into_parts();
        assert!(matches!(
            execution,
            Err(TrustedLuaRuntimeExecutionError::Unavailable(
                TrustedLua54RuntimeError::ShuttingDown
            ))
        ));
        finalization.unwrap();
        assert_eq!(*finalizations.lock().unwrap(), [()]);
    }

    #[derive(Default)]
    struct DocumentedExampleObservations {
        extract_intents: Vec<TrustedLuaStandardExtractIntent>,
        llm_requests: usize,
    }

    struct DocumentedExampleCalls {
        project: LuaProjectContext,
        operations: Arc<RusqliteInteractiveSessionOperations>,
        sources: Arc<HashMap<String, Vec<u8>>>,
        outputs: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        observations: Arc<Mutex<DocumentedExampleObservations>>,
        semantics: Arc<dyn TrustedLuaTranslationSemantics>,
    }

    impl TrustedLuaCommonHostCalls for DocumentedExampleCalls {
        fn project(&self) -> &LuaProjectContext {
            &self.project
        }

        fn read_source(
            &self,
            path: LuaSourcePath,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<Vec<u8>, TrustedLuaHostCallError>> + Send + 'static>,
        > {
            let sources = Arc::clone(&self.sources);
            Box::pin(async move {
                sources.get(path.as_str()).cloned().ok_or_else(|| {
                    TrustedLuaHostCallError::new(
                        "filesystem",
                        "not_found",
                        format!("文档示例来源不存在：{}", path.as_str()),
                        None,
                        None,
                    )
                })
            })
        }

        fn list_source(
            &self,
            path: LuaSourcePath,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<Vec<String>, TrustedLuaHostCallError>> + Send + 'static>,
        > {
            let sources = Arc::clone(&self.sources);
            Box::pin(async move {
                let prefix = format!("{}/", path.as_str());
                let mut entries = sources
                    .keys()
                    .filter_map(|candidate| {
                        candidate
                            .strip_prefix(&prefix)
                            .filter(|relative| !relative.contains('/'))
                            .map(|_| candidate.clone())
                    })
                    .collect::<Vec<_>>();
                entries.sort();
                Ok(entries)
            })
        }

        fn query(
            &self,
            query: SqliteQuery,
        ) -> std::pin::Pin<
            Box<
                dyn Future<Output = Result<Vec<SqliteRow>, TrustedLuaHostCallError>>
                    + Send
                    + 'static,
            >,
        > {
            let operations = Arc::clone(&self.operations);
            Box::pin(async move {
                operations
                    .query(query)
                    .await
                    .map_err(documented_example_database_error)
            })
        }

        fn execute(
            &self,
            command: SqliteCommand,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<u64, TrustedLuaHostCallError>> + Send + 'static>,
        > {
            let operations = Arc::clone(&self.operations);
            Box::pin(async move {
                operations
                    .execute(command)
                    .await
                    .map_err(documented_example_database_error)
            })
        }

        fn begin(
            &self,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<(), TrustedLuaHostCallError>> + Send + 'static>,
        > {
            let operations = Arc::clone(&self.operations);
            Box::pin(async move {
                operations
                    .begin()
                    .await
                    .map_err(documented_example_database_error)
            })
        }

        fn commit(
            &self,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<(), TrustedLuaHostCallError>> + Send + 'static>,
        > {
            let operations = Arc::clone(&self.operations);
            Box::pin(async move {
                operations
                    .commit()
                    .await
                    .map_err(documented_example_database_error)
            })
        }

        fn rollback(
            &self,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<(), TrustedLuaHostCallError>> + Send + 'static>,
        > {
            let operations = Arc::clone(&self.operations);
            Box::pin(async move {
                operations
                    .rollback()
                    .await
                    .map_err(documented_example_database_error)
            })
        }

        fn transaction_active(
            &self,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<bool, TrustedLuaHostCallError>> + Send + 'static>,
        > {
            let operations = Arc::clone(&self.operations);
            Box::pin(async move {
                operations
                    .transaction_active()
                    .await
                    .map_err(documented_example_database_error)
            })
        }
    }

    fn documented_example_database_error<E>(
        error: SqliteInteractiveSessionError<E>,
    ) -> TrustedLuaHostCallError
    where
        E: Error + Send + Sync + 'static,
    {
        let kind = match &error {
            SqliteInteractiveSessionError::Closed => "closed",
            SqliteInteractiveSessionError::Indeterminate => "indeterminate",
            SqliteInteractiveSessionError::TransactionAlreadyActive => "transaction_already_active",
            SqliteInteractiveSessionError::NoActiveTransaction => "no_active_transaction",
            SqliteInteractiveSessionError::OperationFailed(_) => "operation_failed",
            SqliteInteractiveSessionError::OutcomeUnknown(_) => "outcome_unknown",
        };
        let message = error.to_string();
        TrustedLuaHostCallError::new("sqlite", kind, message, None, Some(Arc::new(error)))
    }

    impl TrustedLuaExtractHostCalls for DocumentedExampleCalls {
        fn replace_standard(&self, snapshot: LuaSnapshot) -> Result<(), TrustedLuaHostCallError> {
            self.observations
                .lock()
                .expect("文档示例观察锁不应中毒")
                .extract_intents
                .push(TrustedLuaStandardExtractIntent::Replace(snapshot));
            Ok(())
        }

        fn clear_standard(&self) -> Result<(), TrustedLuaHostCallError> {
            self.observations
                .lock()
                .expect("文档示例观察锁不应中毒")
                .extract_intents
                .push(TrustedLuaStandardExtractIntent::Deactivate);
            Ok(())
        }
    }

    impl TrustedLuaTranslateHostCalls for DocumentedExampleCalls {
        fn system_prompt(&self) -> &str {
            self.semantics.system_prompt()
        }

        fn source_language(&self) -> &str {
            self.semantics.source_language()
        }

        fn target_language(&self) -> &str {
            self.semantics.target_language()
        }

        fn prepare_translation(
            &self,
            kind: TextGroupKind,
            original: String,
            semantic_context: String,
        ) -> Result<Arc<dyn TrustedLuaPreparedTranslation>, TrustedLuaHostCallError> {
            self.semantics
                .prepare_translation(kind, original, semantic_context)
        }

        fn request_llm(
            &self,
            _messages: Vec<ChatMessage>,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<LlmResponse, TrustedLuaHostCallError>> + Send + 'static>,
        > {
            let observations = Arc::clone(&self.observations);
            Box::pin(async move {
                observations
                    .lock()
                    .expect("文档示例观察锁不应中毒")
                    .llm_requests += 1;
                Ok(LlmResponse::new(
                    "星港",
                    crate::llm::LlmFinishReason::Stop,
                    Some("documented-request".to_owned()),
                    Some("documented-response".to_owned()),
                    Some(LlmUsage::new(4, 2, 6)),
                ))
            })
        }
    }

    impl TrustedLuaWriteBackHostCalls for DocumentedExampleCalls {
        fn read_output(
            &self,
            path: ScopedDirectoryPath,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<Vec<u8>, TrustedLuaHostCallError>> + Send + 'static>,
        > {
            let outputs = Arc::clone(&self.outputs);
            let path = documented_example_output_path(&path);
            Box::pin(async move {
                outputs
                    .lock()
                    .expect("文档示例候选锁不应中毒")
                    .get(&path)
                    .cloned()
                    .ok_or_else(|| {
                        TrustedLuaHostCallError::new(
                            "output",
                            "not_found",
                            format!("文档示例候选不存在：{path}"),
                            None,
                            None,
                        )
                    })
            })
        }

        fn list_output(
            &self,
            _path: ScopedDirectoryPath,
        ) -> std::pin::Pin<
            Box<
                dyn Future<Output = Result<Vec<TrustedLuaOutputEntry>, TrustedLuaHostCallError>>
                    + Send
                    + 'static,
            >,
        > {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn create_output_directory(
            &self,
            _path: ScopedDirectoryPath,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<(), TrustedLuaHostCallError>> + Send + 'static>,
        > {
            Box::pin(async { Ok(()) })
        }

        fn write_output(
            &self,
            path: ScopedDirectoryPath,
            bytes: Vec<u8>,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<(), TrustedLuaHostCallError>> + Send + 'static>,
        > {
            let outputs = Arc::clone(&self.outputs);
            let path = documented_example_output_path(&path);
            Box::pin(async move {
                outputs
                    .lock()
                    .expect("文档示例候选锁不应中毒")
                    .insert(path, bytes);
                Ok(())
            })
        }

        fn remove_output(
            &self,
            path: ScopedDirectoryPath,
        ) -> std::pin::Pin<
            Box<dyn Future<Output = Result<(), TrustedLuaHostCallError>> + Send + 'static>,
        > {
            let outputs = Arc::clone(&self.outputs);
            let path = documented_example_output_path(&path);
            Box::pin(async move {
                outputs
                    .lock()
                    .expect("文档示例候选锁不应中毒")
                    .remove(&path);
                Ok(())
            })
        }

        fn layout(
            &self,
            _region: TrustedLuaWriteBackLayoutRegion,
            pairs: Vec<TrustedLuaWriteBackLayoutPair>,
        ) -> Result<TrustedLuaWriteBackLayoutResult, TrustedLuaHostCallError> {
            Ok(TrustedLuaWriteBackLayoutResult::new(
                crate::rpg_maker::lua::runtime::TrustedLuaWriteBackLayoutStatus::Applied,
                pairs
                    .iter()
                    .map(|pair| pair.translation().unwrap_or(pair.original()).to_owned())
                    .collect(),
                0,
                0,
            ))
        }
    }

    fn documented_example_output_path(path: &ScopedDirectoryPath) -> String {
        path.as_path().to_string_lossy().replace('\\', "/")
    }

    struct DocumentedExampleSessionFinalizer {
        finalizer: RusqliteInteractiveSessionFinalizer,
    }

    impl TrustedLuaBindingFinalizer for DocumentedExampleSessionFinalizer {
        fn finalize(
            self: Box<Self>,
        ) -> std::pin::Pin<
            Box<
                dyn Future<
                        Output = Result<
                            TrustedLuaBindingFinalization,
                            TrustedLuaBindingFinalizationError,
                        >,
                    > + Send
                    + 'static,
            >,
        > {
            let Self { finalizer } = *self;
            Box::pin(async move {
                finalizer
                    .finalize()
                    .await
                    .map(|finalization| {
                        TrustedLuaBindingFinalization::new(finalization.had_unclosed_transaction())
                    })
                    .map_err(|error| {
                        let message = error.to_string();
                        TrustedLuaBindingFinalizationError::new(message, Some(Arc::new(error)))
                    })
            })
        }
    }

    fn documented_example_sqlite_storage() -> RusqliteStorage {
        let nonzero = |value| NonZeroUsize::new(value).expect("测试资源配置必须非零");
        let configuration = RusqliteStorageConfiguration::new(nonzero(1), nonzero(1024 * 1024));
        RusqliteStorage::start(configuration).expect("文档示例 SQLite 根应启动")
    }

    fn documented_example_semantics(revision: &str) -> Arc<dyn TrustedLuaTranslationSemantics> {
        let placeholder_service =
            crate::rpg_maker::translate::placeholder::Pcre2PlaceholderService::new()
                .expect("文档示例内建 Placeholder 应编译");
        let custom_placeholders = placeholder_service
            .compile_custom(Vec::new())
            .expect("文档示例空 Placeholder 集应编译");
        let source_language = Arc::new(crate::language::JapaneseLanguageModule::new(
            crate::language::JapaneseResidualPolicy::new(
                NonZeroUsize::new(1).expect("常量非零"),
                Vec::new(),
            )
            .expect("文档示例日文残留策略应有效"),
            None,
        ));
        let mut fingerprint = crate::fingerprint::Sha256FramedHasher::new(
            b"att.rpg-maker.documented-lua-example-semantics",
        );
        fingerprint.frame(1, revision.as_bytes());
        Arc::new(
            crate::rpg_maker::translate::semantics::ResolvedTranslationSemantics::new(
                crate::rpg_maker::RpgMakerEngine::Mz,
                "把日文翻译为简体中文".to_owned(),
                crate::language::LanguagePair::new(
                    crate::language::LanguageId::parse("ja").expect("源语言应合法"),
                    crate::language::LanguageId::parse("zh-Hans").expect("目标语言应合法"),
                ),
                Arc::new(
                    crate::rpg_maker::translate::planning_resource::CompiledTerminology::empty(),
                ),
                placeholder_service,
                custom_placeholders,
                source_language,
                fingerprint.finish(),
            ),
        )
    }

    fn documented_example_sources() -> Arc<HashMap<String, Vec<u8>>> {
        Arc::new(HashMap::from([
            (
                "data/Items.json".to_owned(),
                r#"[null,{"name":"炎之剑","description":"药水","note":"<Help:炎の剣の説明>"}]"#
                    .as_bytes()
                    .to_vec(),
            ),
            (
                "data/QuestEntries.json".to_owned(),
                r#"[{"id":"arrival","title":"星港へ","description":"港へ向かう。"}]"#
                    .as_bytes()
                    .to_vec(),
            ),
            (
                "data/QuestGraph.json".to_owned(),
                r#"[{"id":"arrival","actorId":1,"mapId":1,"title":"星港へ"}]"#
                    .as_bytes()
                    .to_vec(),
            ),
            (
                "data/QuestIndex.json".to_owned(),
                r#"{"arrival":{"label":"星港へ"}}"#.as_bytes().to_vec(),
            ),
            (
                "data/Actors.json".to_owned(),
                r#"[null,{"name":"航海士"}]"#.as_bytes().to_vec(),
            ),
            (
                "data/Map001.json".to_owned(),
                r#"{"displayName":"第一星港"}"#.as_bytes().to_vec(),
            ),
        ]))
    }

    fn documented_example_source(name: &str) -> &'static str {
        match name {
            "lua-standard-data-file.lua" => {
                include_str!("../../../docs/rpg-maker/examples/lua-standard-data-file.lua")
            }
            "lua-translate-state.lua" => {
                include_str!("../../../docs/rpg-maker/examples/lua-translate-state.lua")
            }
            "lua-idempotent-write-back.lua" => {
                include_str!("../../../docs/rpg-maker/examples/lua-idempotent-write-back.lua")
            }
            "lua-private-tag.lua" => {
                include_str!("../../../docs/rpg-maker/examples/lua-private-tag.lua")
            }
            "lua-complex-protocol.lua" => {
                include_str!("../../../docs/rpg-maker/examples/lua-complex-protocol.lua")
            }
            "lua-accept-standard.lua" => {
                include_str!("../../../docs/rpg-maker/examples/lua-accept-standard.lua")
            }
            other => panic!("未知文档 Lua 示例：{other}"),
        }
    }

    fn documented_example_project(
        workspace: &Path,
        database_path: &Path,
        phase: LuaPhase,
    ) -> LuaProjectContext {
        let opened = OpenedProject::new(
            "documented".parse::<ProjectName>().expect("项目名应合法"),
            workspace.to_path_buf(),
            database_path.to_path_buf(),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            crate::rpg_maker::project::test_layout_profile(),
        );
        match phase {
            LuaPhase::Extract | LuaPhase::Translate | LuaPhase::Project => {
                LuaProjectContext::for_frozen_source(
                    opened.name().as_str(),
                    opened.layout().rpg_maker_layout().engine(),
                    opened.source_root().to_path_buf(),
                    opened.database_path().to_path_buf(),
                    opened.language_pair().clone(),
                )
            }
            LuaPhase::WriteBack => LuaProjectContext::for_write_back_candidate(
                opened.name().as_str(),
                opened.layout().rpg_maker_layout().engine(),
                opened.source_root().to_path_buf(),
                opened.database_path().to_path_buf(),
                opened.language_pair().clone(),
                workspace.join("write-back-candidate"),
            ),
        }
    }

    struct DocumentedExampleRun<'a> {
        workspace: &'a Path,
        database_path: &'a Path,
        script_name: &'a str,
        phase: LuaPhase,
        sources: Arc<HashMap<String, Vec<u8>>>,
        outputs: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        observations: Arc<Mutex<DocumentedExampleObservations>>,
        semantic_revision: &'a str,
    }

    async fn run_documented_example(
        runtime: &TrustedLua54Runtime,
        storage: &RusqliteStorage,
        run: DocumentedExampleRun<'_>,
    ) {
        let opened = storage
            .open_existing(run.database_path.to_path_buf())
            .await
            .expect("每次文档示例阶段应打开一个新的真实 SQLite 连接");
        let (operations, finalizer) = opened.into_parts();
        let calls = Arc::new(DocumentedExampleCalls {
            project: documented_example_project(run.workspace, run.database_path, run.phase),
            operations,
            sources: run.sources,
            outputs: run.outputs,
            observations: run.observations,
            semantics: documented_example_semantics(run.semantic_revision),
        });
        let common_calls: Arc<dyn TrustedLuaCommonHostCalls> = calls.clone();
        let common = TrustedLuaCommonBindings::new(common_calls);
        let finalizer: Box<dyn TrustedLuaBindingFinalizer> =
            Box::new(DocumentedExampleSessionFinalizer { finalizer });
        let bindings = match run.phase {
            LuaPhase::Extract => {
                let phase_calls: Arc<dyn TrustedLuaExtractHostCalls> = calls;
                TrustedLuaRuntimeBindings::extract(common, phase_calls, finalizer)
            }
            LuaPhase::Translate => {
                let phase_calls: Arc<dyn TrustedLuaTranslateHostCalls> = calls;
                TrustedLuaRuntimeBindings::translate(common, phase_calls, finalizer)
            }
            LuaPhase::WriteBack => {
                let phase_calls: Arc<dyn TrustedLuaWriteBackHostCalls> = calls;
                TrustedLuaRuntimeBindings::write_back(common, phase_calls, finalizer)
            }
            LuaPhase::Project => {
                panic!("独立项目 Lua 文档示例使用专门的 Standard fixture")
            }
        };
        let script_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("docs/rpg-maker/examples")
            .join(run.script_name);
        let report = runtime
            .start(
                OwnedLuaProgram::new(
                    script_path,
                    documented_example_source(run.script_name)
                        .as_bytes()
                        .to_vec(),
                ),
                bindings,
            )
            .await;
        let (execution, finalization) = report.into_parts();
        execution
            .unwrap_or_else(|error| panic!("文档示例 {} 原样执行失败：{error}", run.script_name));
        let finalization = finalization.unwrap_or_else(|error| {
            panic!("文档示例 {} SQLite 收尾失败：{error}", run.script_name)
        });
        assert!(
            !finalization.had_unclosed_transaction(),
            "文档示例 {} 不得遗留事务",
            run.script_name
        );
    }

    fn empty_documented_outputs() -> Arc<Mutex<HashMap<String, Vec<u8>>>> {
        Arc::new(Mutex::new(HashMap::new()))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn documented_standard_candidate_example_executes_in_the_real_vm() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());
        let observations = Arc::new(Mutex::new(TestObservations::default()));
        let script_name = "lua-accept-standard.lua";
        let script_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("docs/rpg-maker/examples")
            .join(script_name);
        let report = runtime
            .start(
                OwnedLuaProgram::new(
                    script_path,
                    documented_example_source(script_name).as_bytes().to_vec(),
                ),
                project_bindings(Arc::clone(&observations), Vec::new()),
            )
            .await;
        let (execution, finalization) = report.into_parts();
        execution.expect("Standard 人工验收文档示例应由真实 Lua VM 执行成功");
        finalization.expect("Standard 人工验收文档示例应完成唯一终结");
        {
            let observations = observations.lock().expect("测试观察锁不应中毒");
            assert_eq!(observations.standard_candidates.len(), 1);
            assert_eq!(
                observations.standard_candidates[0].candidate(),
                &TextUnitContent::Value("人工译文".to_owned())
            );
        }
        runtime.shutdown().await.expect("Lua Runtime 应关闭");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn documented_custom_data_file_example_executes_in_the_real_vm() {
        let directory = tempfile::tempdir().expect("应建立文档示例临时目录");
        let database_path = directory.path().join("project.db");
        drop(rusqlite::Connection::open(&database_path).expect("应建立文档示例数据库"));
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());
        let storage = documented_example_sqlite_storage();
        let observations = Arc::new(Mutex::new(DocumentedExampleObservations::default()));

        run_documented_example(
            &runtime,
            &storage,
            DocumentedExampleRun {
                workspace: directory.path(),
                database_path: &database_path,
                script_name: "lua-standard-data-file.lua",
                phase: LuaPhase::Extract,
                sources: documented_example_sources(),
                outputs: empty_documented_outputs(),
                observations: Arc::clone(&observations),
                semantic_revision: "semantics-a",
            },
        )
        .await;

        {
            let observations = observations.lock().expect("文档示例观察锁不应中毒");
            let [TrustedLuaStandardExtractIntent::Replace(snapshot)] =
                observations.extract_intents.as_slice()
            else {
                panic!("自定义 DataFile 示例应声明一次标准快照")
            };
            let [group] = snapshot.groups() else {
                panic!("自定义 DataFile 示例应生成一个组")
            };
            assert!(matches!(
                group.group_location().source(),
                RpgMakerSource::DataFile(file) if file.as_str() == "QuestEntries.json"
            ));
            assert_eq!(group.units().len(), 2);
            assert!(matches!(
                group.units()[0].role(),
                crate::rpg_maker::model::TextUnitRole::Scalar(name) if name.as_str() == "title"
            ));
            assert!(matches!(
                group.units()[1].role(),
                crate::rpg_maker::model::TextUnitRole::Scalar(name)
                    if name.as_str() == "description"
            ));
        }

        runtime.shutdown().await.expect("Lua Runtime 应关闭");
        storage.shutdown().await.expect("SQLite 根应关闭");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn documented_translate_state_and_idempotent_write_back_examples_execute() {
        let directory = tempfile::tempdir().expect("应建立文档示例临时目录");
        let database_path = directory.path().join("project.db");
        drop(rusqlite::Connection::open(&database_path).expect("应建立文档示例数据库"));
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());
        let storage = documented_example_sqlite_storage();
        let observations = Arc::new(Mutex::new(DocumentedExampleObservations::default()));
        let sources = documented_example_sources();

        for semantic_revision in ["semantics-a", "semantics-a"] {
            run_documented_example(
                &runtime,
                &storage,
                DocumentedExampleRun {
                    workspace: directory.path(),
                    database_path: &database_path,
                    script_name: "lua-translate-state.lua",
                    phase: LuaPhase::Translate,
                    sources: Arc::clone(&sources),
                    outputs: empty_documented_outputs(),
                    observations: Arc::clone(&observations),
                    semantic_revision,
                },
            )
            .await;
        }
        assert_eq!(
            observations
                .lock()
                .expect("文档示例观察锁不应中毒")
                .llm_requests,
            1,
            "同语义二跑必须由 state 判为 Current，完全不调用 LLM"
        );
        let state_before_change: String = rusqlite::Connection::open(&database_path)
            .expect("应重开文档示例数据库")
            .query_row(
                "SELECT state FROM lua_example_translation WHERE identity = 'quest:arrival:title'",
                [],
                |row| row.get(0),
            )
            .expect("首轮应原子保存 translation/state");

        run_documented_example(
            &runtime,
            &storage,
            DocumentedExampleRun {
                workspace: directory.path(),
                database_path: &database_path,
                script_name: "lua-translate-state.lua",
                phase: LuaPhase::Translate,
                sources: Arc::clone(&sources),
                outputs: empty_documented_outputs(),
                observations: Arc::clone(&observations),
                semantic_revision: "semantics-b",
            },
        )
        .await;
        assert_eq!(
            observations
                .lock()
                .expect("文档示例观察锁不应中毒")
                .llm_requests,
            2,
            "有效公共语义变化后旧 state 必须失效并重新请求 LLM"
        );
        let state_after_change: String = rusqlite::Connection::open(&database_path)
            .expect("应重开文档示例数据库")
            .query_row(
                "SELECT state FROM lua_example_translation WHERE identity = 'quest:arrival:title'",
                [],
                |row| row.get(0),
            )
            .expect("语义变化后应保存新 state");
        assert_ne!(state_before_change, state_after_change);

        let initial_candidate = HashMap::from([(
            "data/QuestEntries.json".to_owned(),
            sources
                .get("data/QuestEntries.json")
                .expect("应有 QuestEntries 来源")
                .clone(),
        )]);
        let outputs = Arc::new(Mutex::new(initial_candidate.clone()));
        let mut generated = Vec::new();
        for _ in 0..2 {
            *outputs.lock().expect("文档示例候选锁不应中毒") = initial_candidate.clone();
            run_documented_example(
                &runtime,
                &storage,
                DocumentedExampleRun {
                    workspace: directory.path(),
                    database_path: &database_path,
                    script_name: "lua-idempotent-write-back.lua",
                    phase: LuaPhase::WriteBack,
                    sources: Arc::clone(&sources),
                    outputs: Arc::clone(&outputs),
                    observations: Arc::clone(&observations),
                    semantic_revision: "semantics-b",
                },
            )
            .await;
            generated.push(
                outputs
                    .lock()
                    .expect("文档示例候选锁不应中毒")
                    .get("data/QuestEntries.json")
                    .expect("WriteBack 应写回 QuestEntries")
                    .clone(),
            );
        }
        assert_eq!(generated[0], generated[1], "从相同 source 重建必须幂等");
        let materialized: serde_json::Value =
            serde_json::from_slice(&generated[0]).expect("写回结果应为 JSON");
        assert_eq!(materialized[0]["title"], "星港");

        runtime.shutdown().await.expect("Lua Runtime 应关闭");
        storage.shutdown().await.expect("SQLite 根应关闭");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn documented_private_tag_protocol_owns_all_three_phases() {
        let directory = tempfile::tempdir().expect("应建立文档示例临时目录");
        let database_path = directory.path().join("project.db");
        drop(rusqlite::Connection::open(&database_path).expect("应建立文档示例数据库"));
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());
        let storage = documented_example_sqlite_storage();
        let observations = Arc::new(Mutex::new(DocumentedExampleObservations::default()));
        let sources = documented_example_sources();
        let initial_items = sources
            .get("data/Items.json")
            .expect("应有 Items 来源")
            .clone();
        let outputs = Arc::new(Mutex::new(HashMap::from([(
            "data/Items.json".to_owned(),
            initial_items.clone(),
        )])));

        for phase in [LuaPhase::Extract, LuaPhase::Translate, LuaPhase::WriteBack] {
            run_documented_example(
                &runtime,
                &storage,
                DocumentedExampleRun {
                    workspace: directory.path(),
                    database_path: &database_path,
                    script_name: "lua-private-tag.lua",
                    phase,
                    sources: Arc::clone(&sources),
                    outputs: Arc::clone(&outputs),
                    observations: Arc::clone(&observations),
                    semantic_revision: "semantics-a",
                },
            )
            .await;
        }

        assert_eq!(
            observations
                .lock()
                .expect("文档示例观察锁不应中毒")
                .llm_requests,
            1,
            "私有标签 Translate 首跑应请求一次模型"
        );
        let connection =
            rusqlite::Connection::open(&database_path).expect("应重开私有标签示例数据库");
        let (original, expected_value, translation, state): (String, String, String, String) =
            connection
                .query_row(
                    "SELECT original, expected_value, translation, state \
                     FROM lua_private_tag_unit WHERE identity = 'item:1:help:0'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .expect("私有标签协议应保存身份、完整原值和翻译状态");
        assert_eq!(original, "炎の剣の説明");
        assert_eq!(expected_value, "<Help:炎の剣の説明>");
        assert_eq!(translation, "星港");
        assert_eq!(state.len(), 64);
        drop(connection);

        let first = outputs
            .lock()
            .expect("文档示例候选锁不应中毒")
            .get("data/Items.json")
            .expect("私有标签协议应写回 Items")
            .clone();
        let materialized: serde_json::Value =
            serde_json::from_slice(&first).expect("Items 写回应为 JSON");
        assert_eq!(materialized[1]["note"], "<Help:星港>");

        *outputs.lock().expect("文档示例候选锁不应中毒") =
            HashMap::from([("data/Items.json".to_owned(), initial_items)]);
        run_documented_example(
            &runtime,
            &storage,
            DocumentedExampleRun {
                workspace: directory.path(),
                database_path: &database_path,
                script_name: "lua-private-tag.lua",
                phase: LuaPhase::WriteBack,
                sources,
                outputs: Arc::clone(&outputs),
                observations,
                semantic_revision: "semantics-a",
            },
        )
        .await;
        assert_eq!(
            outputs.lock().expect("文档示例候选锁不应中毒")["data/Items.json"],
            first,
            "相同完整原值与私有状态必须幂等重建"
        );

        runtime.shutdown().await.expect("Lua Runtime 应关闭");
        storage.shutdown().await.expect("SQLite 根应关闭");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn documented_complex_protocol_executes_all_three_phases_with_persisted_sqlite_state() {
        let directory = tempfile::tempdir().expect("应建立文档示例临时目录");
        let database_path = directory.path().join("project.db");
        drop(rusqlite::Connection::open(&database_path).expect("应建立文档示例数据库"));
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());
        let storage = documented_example_sqlite_storage();
        let observations = Arc::new(Mutex::new(DocumentedExampleObservations::default()));
        let sources = documented_example_sources();
        let initial_candidate = HashMap::from([
            (
                "data/QuestGraph.json".to_owned(),
                sources
                    .get("data/QuestGraph.json")
                    .expect("应有 QuestGraph 来源")
                    .clone(),
            ),
            (
                "data/QuestIndex.json".to_owned(),
                sources
                    .get("data/QuestIndex.json")
                    .expect("应有 QuestIndex 来源")
                    .clone(),
            ),
        ]);
        let outputs = Arc::new(Mutex::new(initial_candidate.clone()));

        for phase in [LuaPhase::Extract, LuaPhase::Translate, LuaPhase::WriteBack] {
            run_documented_example(
                &runtime,
                &storage,
                DocumentedExampleRun {
                    workspace: directory.path(),
                    database_path: &database_path,
                    script_name: "lua-complex-protocol.lua",
                    phase,
                    sources: Arc::clone(&sources),
                    outputs: Arc::clone(&outputs),
                    observations: Arc::clone(&observations),
                    semantic_revision: "semantics-a",
                },
            )
            .await;
        }
        assert_eq!(
            observations
                .lock()
                .expect("文档示例观察锁不应中毒")
                .llm_requests,
            1
        );

        let connection =
            rusqlite::Connection::open(&database_path).expect("应重开复杂协议示例数据库");
        let (translation, state): (String, String) = connection
            .query_row(
                "SELECT translation, state FROM lua_complex_unit WHERE identity = 'quest:arrival'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("复杂协议应保存翻译状态");
        assert_eq!(translation, "星港");
        assert_eq!(state.len(), 64);
        let target_count: i64 = connection
            .query_row(
                "SELECT count(*) FROM lua_complex_target WHERE identity = 'quest:arrival'",
                [],
                |row| row.get(0),
            )
            .expect("复杂协议应保存两个有序写回目标");
        assert_eq!(target_count, 2);
        drop(connection);

        let first_graph = outputs
            .lock()
            .expect("文档示例候选锁不应中毒")
            .get("data/QuestGraph.json")
            .expect("复杂协议应写 QuestGraph")
            .clone();
        let first_index = outputs
            .lock()
            .expect("文档示例候选锁不应中毒")
            .get("data/QuestIndex.json")
            .expect("复杂协议应写 QuestIndex")
            .clone();
        let graph: serde_json::Value =
            serde_json::from_slice(&first_graph).expect("QuestGraph 写回应为 JSON");
        let index: serde_json::Value =
            serde_json::from_slice(&first_index).expect("QuestIndex 写回应为 JSON");
        assert_eq!(graph[0]["title"], "星港");
        assert_eq!(index["arrival"]["label"], "星港");

        *outputs.lock().expect("文档示例候选锁不应中毒") = initial_candidate;
        run_documented_example(
            &runtime,
            &storage,
            DocumentedExampleRun {
                workspace: directory.path(),
                database_path: &database_path,
                script_name: "lua-complex-protocol.lua",
                phase: LuaPhase::WriteBack,
                sources,
                outputs: Arc::clone(&outputs),
                observations,
                semantic_revision: "semantics-a",
            },
        )
        .await;
        {
            let outputs = outputs.lock().expect("文档示例候选锁不应中毒");
            assert_eq!(outputs["data/QuestGraph.json"], first_graph);
            assert_eq!(outputs["data/QuestIndex.json"], first_index);
        }

        runtime.shutdown().await.expect("Lua Runtime 应关闭");
        storage.shutdown().await.expect("SQLite 根应关闭");
    }
}
