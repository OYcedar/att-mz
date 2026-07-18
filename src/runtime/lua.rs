//! 使用专用 OS worker 运行完整 Lua 5.4 的生产根适配器。

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::ffi::CString;
use std::fmt;
use std::future::Future;
use std::io;
use std::num::{NonZeroU32, NonZeroUsize};
use std::os::windows::ffi::OsStrExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::rc::Rc;
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
use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

use crate::att_mz::extract::store::{
    ExtractedTextField, ExtractedTextGroup, LuaSnapshot, SnapshotModelError,
};
use crate::att_mz::lua::json::{
    HostValueBudget, HostValueBudgetExceeded, HostValueBudgetTracker, LosslessJsonValue,
    decode as decode_json, validate_number,
};
use crate::att_mz::lua::mz::{
    MzDocumentError, MzTextReference, OpenedMzDocument, data_source, map_source,
    plugin_parameter_source, source_path,
};
use crate::att_mz::lua::runtime::{
    OwnedLuaProgram, TrustedLuaBindingFinalizationError, TrustedLuaBindingFinalizer,
    TrustedLuaCommonBindings, TrustedLuaCommonHostCalls, TrustedLuaExecutionHandle,
    TrustedLuaExtractHostCalls, TrustedLuaHostCallError, TrustedLuaOutputEntry,
    TrustedLuaPhaseBindings, TrustedLuaPreparedTranslation,
    TrustedLuaPreparedTranslationAcceptance, TrustedLuaRuntimeBindings,
    TrustedLuaRuntimeExecutionError, TrustedLuaRuntimeExecutionReport, TrustedLuaRuntimeExecutor,
    TrustedLuaTranslateHostCalls, TrustedLuaWriteBackHostCalls, TrustedLuaWriteBackLayoutPair,
    TrustedLuaWriteBackLayoutRegion, TrustedLuaWriteBackLayoutResult,
};
use crate::att_mz::lua::{LuaPhase, LuaProjectContext, LuaSourcePath};
use crate::att_mz::standard_asset::MzStandardAssetStorageKind;
use crate::att_mz::text::{MzLocation, MzLocationStep, MzSource, TextGroupKind};
use crate::llm::{ChatMessage, ChatMessageRole, LlmResponse, LlmUsage};
use crate::storage::file_system::ScopedDirectoryPath;
use crate::storage::sqlite::{SqliteCommand, SqliteQuery, SqliteRow, SqliteValue};

/// 已由配置边界建立的单次 Lua 执行资源上限。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TrustedLua54RuntimeConfiguration {
    worker_stack_bytes: NonZeroUsize,
    memory_limit_bytes_per_vm: NonZeroUsize,
    cancel_check_instruction_interval: NonZeroU32,
    max_error_bytes: NonZeroUsize,
    host_value_budget: HostValueBudget,
}

impl TrustedLua54RuntimeConfiguration {
    pub(crate) const fn new(
        worker_stack_bytes: NonZeroUsize,
        memory_limit_bytes_per_vm: NonZeroUsize,
        cancel_check_instruction_interval: NonZeroU32,
        max_error_bytes: NonZeroUsize,
        host_value_budget: HostValueBudget,
    ) -> Self {
        Self {
            worker_stack_bytes,
            memory_limit_bytes_per_vm,
            cancel_check_instruction_interval,
            max_error_bytes,
            host_value_budget,
        }
    }
}

/// Lua 生产根的线程启动、生命周期或 VM 失败。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TrustedLua54RuntimeError {
    WorkerSpawn(String),
    ShuttingDown,
    WorkerChannelClosed,
    Context(String),
    Vm(String),
}

impl fmt::Display for TrustedLua54RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkerSpawn(message) => write!(formatter, "无法创建 Lua worker：{message}"),
            Self::ShuttingDown => formatter.write_str("Lua Runtime 正在关闭"),
            Self::WorkerChannelClosed => formatter.write_str("Lua worker 通道已关闭"),
            Self::Context(message) => write!(formatter, "Lua 上下文无效：{message}"),
            Self::Vm(message) => formatter.write_str(message),
        }
    }
}

impl Error for TrustedLua54RuntimeError {}

struct RuntimeInner {
    lifecycle: Mutex<RuntimeLifecycle>,
    shutdown_requested: Arc<AtomicBool>,
    runtime_handles: AtomicUsize,
    jobs_finished: Notify,
    tokio: Handle,
    worker_stack_bytes: usize,
    memory_limit_bytes_per_vm: usize,
    cancel_check_instruction_interval: u32,
    max_error_bytes: usize,
    host_value_budget: HostValueBudget,
}

struct RuntimeLifecycle {
    accepting: bool,
    active_jobs: usize,
}

/// 进程内完整 Lua 5.4 Runtime。
///
/// VM 与全部 Lua 标准库只在专用 OS worker 中存在。SQLite 与 LLM 调用通过
/// 同步响应桥交回构造本根的 Tokio Runtime 驱动。
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
                memory_limit_bytes_per_vm: configuration.memory_limit_bytes_per_vm.get(),
                cancel_check_instruction_interval: configuration
                    .cancel_check_instruction_interval
                    .get(),
                max_error_bytes: configuration.max_error_bytes.get(),
                host_value_budget: configuration.host_value_budget,
            }),
        }
    }

    /// 停止新启动，取消正在执行的脚本，并等待 worker 与唯一终结器退出。
    ///
    /// 可信脚本进入 native C 模块、`os.execute` 或替换调试 hook 后可以长时间不
    /// 交还控制；本方法不伪造超时成功。
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
        let limits = LuaExecutionLimits {
            memory_limit_bytes_per_vm: self.inner.memory_limit_bytes_per_vm,
            cancel_check_instruction_interval: self.inner.cancel_check_instruction_interval,
            max_error_bytes: self.inner.max_error_bytes,
            host_value_budget: self.inner.host_value_budget,
        };

        let worker = if accepted {
            let sender = Arc::new(Mutex::new(Some(worker_sender)));
            let worker_sender = Arc::clone(&sender);
            let worker = thread::Builder::new()
                .name("att-lua".to_owned())
                .stack_size(self.inner.worker_stack_bytes)
                .spawn(move || {
                    let outcome = catch_unwind(AssertUnwindSafe(|| {
                        execute_program(&program, common, phase, &tokio, &cancellation, limits)
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
                            TrustedLua54RuntimeError::WorkerSpawn(error.to_string()),
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
struct LuaExecutionLimits {
    memory_limit_bytes_per_vm: usize,
    cancel_check_instruction_interval: u32,
    max_error_bytes: usize,
    host_value_budget: HostValueBudget,
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
    limits: LuaExecutionLimits,
) -> WorkerOutcome {
    let LuaExecutionLimits {
        memory_limit_bytes_per_vm,
        cancel_check_instruction_interval,
        max_error_bytes,
        host_value_budget,
    } = limits;
    if cancellation.is_cancelled() {
        return WorkerOutcome::Cancelled;
    }

    let (script_path, script_directory) = match strict_script_paths(program.main_script_path()) {
        Ok(paths) => paths,
        Err(error) => return WorkerOutcome::Context(error),
    };

    let native_modules = Rc::new(NativeModuleRegistry::default());
    // SAFETY: 脚本是用户明确选择的完全可信本机程序；契约明确允许 debug、io、os、
    // require 与本地 C 模块。VM 只在当前专用 worker 线程中创建、使用和销毁。
    // native_modules 在 lua 之前声明，因此所有动态库一定晚于 VM 和其中的 C Function 释放。
    let lua = unsafe { Lua::unsafe_new_with(StdLib::ALL, LuaOptions::default()) };
    if let Err(error) = lua.set_memory_limit(memory_limit_bytes_per_vm) {
        return WorkerOutcome::Context(vm_error(
            "无法设置 Lua VM 内存上限",
            error,
            max_error_bytes,
        ));
    }

    if let Err(error) = configure_module_paths(&lua, &script_directory, Rc::clone(&native_modules))
    {
        return WorkerOutcome::Context(vm_error(
            "无法配置 Lua package 路径",
            error,
            max_error_bytes,
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
        return WorkerOutcome::Context(vm_error("无法安装 Lua 取消 hook", error, max_error_bytes));
    }
    if cancellation.is_cancelled() {
        return WorkerOutcome::Cancelled;
    }

    let context = match build_context(
        &lua,
        common,
        phase,
        tokio.clone(),
        cancellation.clone(),
        host_value_budget,
    ) {
        Ok(context) => context,
        Err(error) => {
            if cancellation.is_cancelled() {
                return WorkerOutcome::Cancelled;
            }
            return WorkerOutcome::Context(vm_error("无法构造 Lua ctx", error, max_error_bytes));
        }
    };
    if let Err(error) = lua.globals().set("ctx", context) {
        if cancellation.is_cancelled() {
            return WorkerOutcome::Cancelled;
        }
        return WorkerOutcome::Context(vm_error("无法注入 Lua ctx", error, max_error_bytes));
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
            return WorkerOutcome::Compile(vm_error("Lua 主程序编译失败", error, max_error_bytes));
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
                "无法构造 Lua 执行边界",
                error,
                max_error_bytes,
            ));
        }
    };

    let (succeeded, error): (bool, Value) = match runner.call(function) {
        Ok(result) => result,
        Err(error) => {
            if cancellation.is_cancelled() {
                return WorkerOutcome::Cancelled;
            }
            return WorkerOutcome::Execute(vm_error("Lua 主程序运行失败", error, max_error_bytes));
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
    WorkerOutcome::Execute(TrustedLua54RuntimeError::Vm(truncate_utf8(
        format!("Lua 主程序运行失败：{}", lua_value_description(&error)),
        max_error_bytes,
    )))
}

fn strict_script_paths(path: &Path) -> Result<(String, String), TrustedLua54RuntimeError> {
    let script_path = path
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| TrustedLua54RuntimeError::Context("Lua 主程序路径不是 UTF-8".to_owned()))?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let parent = parent
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| TrustedLua54RuntimeError::Context("Lua 主程序目录不是 UTF-8".to_owned()))?;
    Ok((script_path, parent))
}

fn configure_module_paths(
    lua: &Lua,
    script_directory: &str,
    native_modules: Rc<NativeModuleRegistry>,
) -> mlua::Result<()> {
    let package: Table = lua.globals().get("package")?;
    let current_path: String = package.get("path")?;
    let current_cpath: String = package.get("cpath")?;
    let separator = if script_directory.ends_with(['/', '\\']) {
        ""
    } else if cfg!(windows) {
        "\\"
    } else {
        "/"
    };
    package.set(
        "path",
        format!(
            "{script_directory}{separator}?.lua;{script_directory}{separator}?{separator}init.lua;{current_path}"
        ),
    )?;
    package.set(
        "cpath",
        format!("{script_directory}{separator}?.dll;{current_cpath}"),
    )?;
    install_unicode_module_searchers(lua, &package, native_modules)
}

fn install_unicode_module_searchers(
    lua: &Lua,
    package: &Table,
    native_modules: Rc<NativeModuleRegistry>,
) -> mlua::Result<()> {
    let lua_searcher = lua.create_function(move |lua, module: mlua::LuaString| {
        let module = strict_module_name(&module)?;
        let candidates = package_candidates(lua, "path", &module)?;
        let mut diagnostics = String::new();
        for candidate in candidates {
            match std::fs::read(&candidate) {
                Ok(source) => {
                    let name = strict_module_path(&candidate)?;
                    let loader = lua.load(source).set_name(name).into_function()?;
                    return Ok(MultiValue::from_vec(vec![
                        Value::Function(loader),
                        Value::String(lua.create_string(name)?),
                    ]));
                }
                Err(error) => {
                    use std::fmt::Write as _;
                    let name = strict_module_path(&candidate)?;
                    let _ = write!(diagnostics, "\n\tno file '{name}' ({error})");
                }
            }
        }
        Ok(MultiValue::from_vec(vec![Value::String(
            lua.create_string(diagnostics)?,
        )]))
    })?;

    let direct_modules = Rc::clone(&native_modules);
    let direct_c_searcher = lua.create_function(move |lua, module: mlua::LuaString| {
        let module = strict_module_name(&module)?;
        let candidates = package_candidates(lua, "cpath", &module)?;
        native_module_search(lua, &direct_modules, &module, candidates, false)
    })?;

    let root_c_searcher = lua.create_function(move |lua, module: mlua::LuaString| {
        let module = strict_module_name(&module)?;
        let Some((root, _)) = module.split_once('.') else {
            return Ok(MultiValue::new());
        };
        let candidates = package_candidates(lua, "cpath", root)?;
        native_module_search(lua, &native_modules, &module, candidates, true)
    })?;

    let current_searchers: Table = package.get("searchers")?;
    let preload: Value = current_searchers.raw_get(1)?;
    let searchers = lua.create_table()?;
    searchers.raw_set(1, preload)?;
    searchers.raw_set(2, lua_searcher)?;
    searchers.raw_set(3, direct_c_searcher)?;
    searchers.raw_set(4, root_c_searcher)?;
    package.set("searchers", searchers)
}

fn strict_module_name(module: &mlua::LuaString) -> mlua::Result<String> {
    module
        .to_str()
        .map(|module| module.to_owned())
        .map_err(|_| mlua::Error::runtime("Lua 模块名不是 UTF-8"))
}

fn strict_module_path(path: &Path) -> mlua::Result<&str> {
    path.to_str()
        .ok_or_else(|| mlua::Error::runtime("Lua 模块路径无法无损转换为 UTF-8"))
}

fn package_candidates(lua: &Lua, field: &str, module: &str) -> mlua::Result<Vec<PathBuf>> {
    let package: Table = lua.globals().get("package")?;
    let templates: mlua::LuaString = package.get(field)?;
    let templates = templates
        .to_str()
        .map_err(|_| mlua::Error::runtime(format!("package.{field} 不是 UTF-8 字符串")))?;
    let module_path = module.replace('.', "\\");
    Ok(templates
        .split(';')
        .filter(|template| !template.is_empty())
        .map(|template| PathBuf::from(template.replace('?', &module_path)))
        .collect())
}

fn native_module_search(
    lua: &Lua,
    registry: &NativeModuleRegistry,
    module: &str,
    candidates: Vec<PathBuf>,
    missing_symbol_is_diagnostic: bool,
) -> mlua::Result<MultiValue> {
    let mut diagnostics = String::new();
    for candidate in candidates {
        match std::fs::metadata(&candidate) {
            Ok(metadata) if metadata.is_file() => {
                match registry.load_function(&candidate, module) {
                    Ok((function, loaded_path)) => {
                        // SAFETY: load_function 只返回仍由 registry 持有的 HMODULE 中、
                        // 按 Lua 5.4 luaopen_* 契约解析出的入口；本能力只运行用户明确
                        // 信任的本机模块，且 registry 的寿命覆盖 VM。
                        let loader = unsafe { lua.create_c_function(function) }?;
                        let loaded_path = strict_module_path(&loaded_path)?;
                        return Ok(MultiValue::from_vec(vec![
                            Value::Function(loader),
                            Value::String(lua.create_string(loaded_path)?),
                        ]));
                    }
                    Err(NativeModuleLoadError::MissingSymbol { path, .. })
                        if missing_symbol_is_diagnostic =>
                    {
                        use std::fmt::Write as _;
                        let path = strict_module_path(&path)?;
                        let _ = write!(diagnostics, "\n\tno module '{module}' in file '{path}'");
                        return Ok(MultiValue::from_vec(vec![Value::String(
                            lua.create_string(diagnostics)?,
                        )]));
                    }
                    Err(error) => return Err(mlua::Error::runtime(error.to_string())),
                }
            }
            Ok(_) => {
                use std::fmt::Write as _;
                let path = strict_module_path(&candidate)?;
                let _ = write!(diagnostics, "\n\tno file '{path}' (not a regular file)");
            }
            Err(error) => {
                use std::fmt::Write as _;
                let path = strict_module_path(&candidate)?;
                let _ = write!(diagnostics, "\n\tno file '{path}' ({error})");
            }
        }
    }
    Ok(MultiValue::from_vec(vec![Value::String(
        lua.create_string(diagnostics)?,
    )]))
}

#[derive(Default)]
struct NativeModuleRegistry {
    handles: RefCell<HashMap<PathBuf, HMODULE>>,
}

impl NativeModuleRegistry {
    fn load_function(
        &self,
        path: &Path,
        module: &str,
    ) -> Result<(mlua::lua_CFunction, PathBuf), NativeModuleLoadError> {
        let path = std::fs::canonicalize(path).map_err(|source| NativeModuleLoadError::Load {
            path: path.to_path_buf(),
            source,
        })?;
        path.to_str().ok_or(NativeModuleLoadError::InvalidPath)?;

        let handle = if let Some(handle) = self.handles.borrow().get(&path).copied() {
            handle
        } else {
            let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
            if wide.contains(&0) {
                return Err(NativeModuleLoadError::InvalidPath);
            }
            wide.push(0);
            // SAFETY: wide 是以 NUL 结尾且在调用期间有效的 UTF-16 路径；调用方只
            // 允许完全可信的本机模块。成功返回的 HMODULE 被本 registry 唯一持有，
            // 并在 VM 完全销毁后释放。
            let handle = unsafe { LoadLibraryW(wide.as_ptr()) };
            if handle.is_null() {
                return Err(NativeModuleLoadError::Load {
                    path,
                    source: io::Error::last_os_error(),
                });
            }
            self.handles.borrow_mut().insert(path.clone(), handle);
            handle
        };

        let symbols = native_module_symbols(module);
        for symbol in &symbols {
            let symbol_name = CString::new(symbol.as_str()).map_err(|_| {
                NativeModuleLoadError::InvalidSymbol {
                    module: module.to_owned(),
                }
            })?;
            // SAFETY: handle 是本 registry 仍持有的可信模块 HMODULE；symbol_name
            // 是 NUL 结尾且在调用期间有效的 ASCII/UTF-8 导出名。
            let address = unsafe { GetProcAddress(handle, symbol_name.as_ptr().cast()) };
            if let Some(address) = address {
                // SAFETY: 仅支持 x86_64-pc-windows-msvc，该平台 system 与 C 调用约定
                // 相同；用户信任的 luaopen_* 导出按 Lua C API 接受 lua_State 并返回
                // 结果数量，且其 HMODULE 在整个 VM 生命周期内保持有效。
                let function = unsafe {
                    std::mem::transmute::<unsafe extern "system" fn() -> isize, mlua::lua_CFunction>(
                        address,
                    )
                };
                return Ok((function, path));
            }
        }
        Err(NativeModuleLoadError::MissingSymbol { path, symbols })
    }
}

impl Drop for NativeModuleRegistry {
    fn drop(&mut self) {
        for (_, handle) in self.handles.get_mut().drain() {
            // SAFETY: 每个 handle 都来自一次成功的 LoadLibraryW，只在此处释放一次；
            // registry 的 drop 晚于 Lua VM，因此已不存在引用这些导出的 Lua Function。
            // FreeLibrary 触发的可信模块卸载代码也在其最后有效引用消失后运行。
            let _ = unsafe { FreeLibrary(handle) };
        }
    }
}

fn native_module_symbols(module: &str) -> Vec<String> {
    let normalized = module.replace('.', "_");
    match normalized.split_once('-') {
        Some((prefix, suffix)) => vec![format!("luaopen_{prefix}"), format!("luaopen_{suffix}")],
        None => vec![format!("luaopen_{normalized}")],
    }
}

enum NativeModuleLoadError {
    InvalidPath,
    InvalidSymbol { module: String },
    Load { path: PathBuf, source: io::Error },
    MissingSymbol { path: PathBuf, symbols: Vec<String> },
}

impl fmt::Display for NativeModuleLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPath => {
                formatter.write_str("Lua C 模块路径无法无损转换为 UTF-8 或包含 NUL")
            }
            Self::InvalidSymbol { module } => {
                write!(formatter, "Lua C 模块名无法形成导出符号：{module}")
            }
            Self::Load { path, source } => {
                write!(
                    formatter,
                    "无法加载 Lua C 模块 {}：{source}",
                    path.display()
                )
            }
            Self::MissingSymbol { path, symbols } => write!(
                formatter,
                "Lua C 模块 {} 缺少导出符号 {}",
                path.display(),
                symbols.join(" 或 ")
            ),
        }
    }
}

fn build_context(
    lua: &Lua,
    common: TrustedLuaCommonBindings,
    phase: TrustedLuaPhaseBindings,
    tokio: Handle,
    cancellation: RuntimeCancellation,
    host_value_budget: HostValueBudget,
) -> mlua::Result<Table> {
    let phase_name = phase_name(phase.phase());
    let calls = Arc::clone(common.calls());
    let context = lua.create_table()?;
    let json_markers = lua.create_table()?;
    context.set("phase", phase_name)?;
    context.set("project", build_project_table(lua, calls.project())?)?;
    context.set(
        "json",
        build_json_table(lua, host_value_budget, &json_markers)?,
    )?;
    context.set(
        "source",
        build_source_table(
            lua,
            Arc::clone(&calls),
            tokio.clone(),
            cancellation.clone(),
            host_value_budget,
            &json_markers,
        )?,
    )?;
    context.set(
        "mz",
        build_mz_table(
            lua,
            Arc::clone(&calls),
            tokio.clone(),
            cancellation.clone(),
            host_value_budget,
            &json_markers,
        )?,
    )?;
    context.set(
        "db",
        build_database_table(
            lua,
            Arc::clone(&calls),
            tokio.clone(),
            cancellation.clone(),
            host_value_budget,
        )?,
    )?;
    match phase {
        TrustedLuaPhaseBindings::Extract(extract) => {
            install_extract_context(lua, &context, extract, host_value_budget)?;
        }
        TrustedLuaPhaseBindings::Translate(translate) => {
            install_translate_context(
                lua,
                &context,
                translate,
                tokio,
                cancellation,
                host_value_budget,
            )?;
        }
        TrustedLuaPhaseBindings::WriteBack(write_back) => {
            install_write_back_context(
                lua,
                &context,
                write_back,
                tokio,
                cancellation,
                host_value_budget,
                &json_markers,
            )?;
        }
    }
    Ok(context)
}

fn install_extract_context(
    lua: &Lua,
    context: &Table,
    calls: Arc<dyn TrustedLuaExtractHostCalls>,
    budget: HostValueBudget,
) -> mlua::Result<()> {
    let extract = lua.create_table()?;
    let declared = Arc::new(AtomicBool::new(false));
    let replace_calls = Arc::clone(&calls);
    let replace_declared = Arc::clone(&declared);
    let replace = lua.create_function(move |lua, groups: Value| {
        let result = parse_lua_standard_snapshot(groups, budget).and_then(|snapshot| {
            claim_extract_intent(&replace_declared)?;
            replace_calls.replace_standard(snapshot)
        });
        host_result_to_lua(lua, result, |_, ()| Ok(Value::Nil))
    })?;
    extract.set("replace_standard", checked_host_function(lua, replace)?)?;

    let clear = lua.create_function(move |lua, ()| {
        let result = claim_extract_intent(&declared).and_then(|()| calls.clear_standard());
        host_result_to_lua(lua, result, |_, ()| Ok(Value::Nil))
    })?;
    extract.set("clear_standard", checked_host_function(lua, clear)?)?;
    context.set("extract", extract)?;
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

fn parse_lua_standard_snapshot(
    value: Value,
    budget: HostValueBudget,
) -> Result<LuaSnapshot, TrustedLuaHostCallError> {
    let Value::Table(groups) = value else {
        return Err(extract_argument_error(format!(
            "extract.replace_standard groups 必须是无洞数组，实际为 {}",
            value.type_name()
        )));
    };
    let mut tracker = HostValueBudgetTracker::new(budget);
    tracker
        .container(1)
        .map_err(|error| host_value_budget_error("Extract 标准快照", error))?;
    let groups = dense_values_bounded(groups, budget, "Extract 标准快照", |error| {
        extract_argument_error(error.to_string())
    })?
    .into_iter()
    .map(|value| parse_lua_standard_group(value, &mut tracker, 2, budget))
    .collect::<Result<Vec<_>, _>>()?;
    LuaSnapshot::new(groups).map_err(snapshot_model_error)
}

fn parse_lua_standard_group(
    value: Value,
    tracker: &mut HostValueBudgetTracker,
    depth: usize,
    budget: HostValueBudget,
) -> Result<ExtractedTextGroup, TrustedLuaHostCallError> {
    tracker
        .container(depth)
        .map_err(|error| host_value_budget_error("Extract 标准快照", error))?;
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
        tracker,
        depth + 1,
    )?;
    let group_location = parse_lua_mz_location(
        group
            .get("location")
            .map_err(|error| extract_argument_error(error.to_string()))?,
        "group.location",
    )?;
    track_mz_location(tracker, depth + 1, &group_location)
        .map_err(|error| host_value_budget_error("Extract 标准快照", error))?;
    if !matches!(group_location, MzLocation::Value { .. }) {
        return Err(extract_argument_error(
            "extract group.location 必须是 document:location 建立的 Value 地址".to_owned(),
        ));
    }
    let fields: Value = group
        .get("fields")
        .map_err(|error| extract_argument_error(error.to_string()))?;
    let Value::Table(fields) = fields else {
        return Err(extract_argument_error(format!(
            "extract group.fields 必须是无洞数组，实际为 {}",
            fields.type_name()
        )));
    };
    tracker
        .container(depth + 1)
        .map_err(|error| host_value_budget_error("Extract 标准快照", error))?;
    let storage = MzStandardAssetStorageKind::for_group_kind(kind);
    let fields = dense_values_bounded(fields, budget, "Extract 标准快照", |error| {
        extract_argument_error(error.to_string())
    })?
    .into_iter()
    .map(|value| parse_lua_standard_field(value, storage, &group_location, tracker, depth + 2))
    .collect::<Result<Vec<_>, _>>()?;
    ExtractedTextGroup::new(kind, group_location, fields).map_err(snapshot_model_error)
}

fn parse_lua_standard_field(
    value: Value,
    storage: MzStandardAssetStorageKind,
    group_location: &MzLocation,
    tracker: &mut HostValueBudgetTracker,
    depth: usize,
) -> Result<ExtractedTextField, TrustedLuaHostCallError> {
    tracker
        .container(depth)
        .map_err(|error| host_value_budget_error("Extract 标准快照", error))?;
    let Value::Table(field) = value else {
        return Err(extract_argument_error(format!(
            "extract group.fields 的每一项必须是 table，实际为 {}",
            value.type_name()
        )));
    };
    ensure_exact_string_keys(&field, &["name", "text"])
        .map_err(|error| extract_argument_error(error.to_string()))?;
    let name = parse_mz_string(
        field
            .get("name")
            .map_err(|error| extract_argument_error(error.to_string()))?,
        "extract field.name",
    )
    .map_err(|error| extract_argument_error(error.to_string()))?;
    tracker
        .string(depth + 1, &name)
        .map_err(|error| host_value_budget_error("Extract 标准快照", error))?;
    let text: Value = field
        .get("text")
        .map_err(|error| extract_argument_error(error.to_string()))?;
    let Value::UserData(text) = text else {
        return Err(extract_argument_error(format!(
            "extract field.text 必须是 MzDocument 建立的 MzTextRef，实际为 {}",
            text.type_name()
        )));
    };
    if !text.is::<LuaMzTextReference>() {
        return Err(extract_argument_error(
            "extract field.text 必须是 MzDocument 建立的 MzTextRef".to_owned(),
        ));
    }
    let text = text
        .borrow::<LuaMzTextReference>()
        .map_err(|error| extract_argument_error(error.to_string()))?
        .0
        .clone();
    track_mz_text_reference(tracker, depth + 1, &text)
        .map_err(|error| host_value_budget_error("Extract 标准快照", error))?;
    storage
        .validate_locations(text.location(), group_location)
        .map_err(|error| extract_argument_error(error.to_string()))?;
    ExtractedTextField::new(name, text.location().clone(), text.original().to_owned())
        .map_err(snapshot_model_error)
}

fn parse_lua_mz_location(value: Value, role: &str) -> Result<MzLocation, TrustedLuaHostCallError> {
    let Value::UserData(value) = value else {
        return Err(extract_argument_error(format!(
            "extract {role} 必须是 MzDocument 建立的位置，实际为 {}",
            value.type_name()
        )));
    };
    if !value.is::<LuaMzLocation>() {
        return Err(extract_argument_error(format!(
            "extract {role} 必须是 MzDocument 建立的位置"
        )));
    }
    value
        .borrow::<LuaMzLocation>()
        .map(|location| location.0.clone())
        .map_err(|error| extract_argument_error(error.to_string()))
}

fn parse_standard_group_kind(
    value: Value,
    tracker: &mut HostValueBudgetTracker,
    depth: usize,
) -> Result<TextGroupKind, TrustedLuaHostCallError> {
    let kind = parse_mz_string(value, "extract group.kind")
        .map_err(|error| extract_argument_error(error.to_string()))?;
    tracker
        .string(depth, &kind)
        .map_err(|error| host_value_budget_error("Extract 标准快照", error))?;
    match kind.as_str() {
        "database_entry" => Ok(TextGroupKind::DatabaseEntry),
        "system" => Ok(TextGroupKind::System),
        "map" => Ok(TextGroupKind::Map),
        "dialogue" => Ok(TextGroupKind::EventDialogue),
        "choices" => Ok(TextGroupKind::EventChoices),
        "scrolling_text" => Ok(TextGroupKind::EventScrollingText),
        "event_command" => Ok(TextGroupKind::EventCommand),
        "plugin_parameter" => Ok(TextGroupKind::PluginParameter),
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

fn install_translate_context(
    lua: &Lua,
    context: &Table,
    calls: Arc<dyn TrustedLuaTranslateHostCalls>,
    tokio: Handle,
    cancellation: RuntimeCancellation,
    budget: HostValueBudget,
) -> mlua::Result<()> {
    context.set(
        "translation",
        build_translation_table(lua, Arc::clone(&calls), budget)?,
    )?;
    context.set(
        "llm",
        build_llm_function(lua, calls, tokio, cancellation, budget)?,
    )
}

fn build_translation_table(
    lua: &Lua,
    calls: Arc<dyn TrustedLuaTranslateHostCalls>,
    budget: HostValueBudget,
) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("system_prompt", calls.system_prompt())?;
    let language_pair = lua.create_table()?;
    language_pair.set("source", calls.source_language())?;
    language_pair.set("target", calls.target_language())?;
    table.set("language_pair", language_pair)?;

    let native = lua.create_function(move |lua, (kind, original): (Value, Value)| {
        let result = parse_translation_prepare(kind, original, budget)
            .and_then(|(kind, original)| calls.prepare_translation(kind, original))
            .and_then(|prepared| {
                validate_prepared_translation_budget(prepared.as_ref(), budget)?;
                Ok(prepared)
            });
        host_result_to_lua(lua, result, |lua, prepared| {
            prepared_translation_to_lua(lua, prepared, budget)
        })
    })?;
    table.set("prepare", checked_host_function(lua, native)?)?;
    Ok(table)
}

fn parse_translation_prepare(
    kind: Value,
    original: Value,
    budget: HostValueBudget,
) -> Result<(TextGroupKind, String), TrustedLuaHostCallError> {
    let Value::String(kind) = kind else {
        return Err(binding_error(mlua::Error::runtime(format!(
            "translation.prepare kind 必须是字符串，实际为 {}",
            kind.type_name()
        ))));
    };
    let kind_name = lua_string_to_text(&kind, "translation.prepare kind").map_err(binding_error)?;
    let kind = match kind_name.as_str() {
        "database_entry" => TextGroupKind::DatabaseEntry,
        "system" => TextGroupKind::System,
        "map" => TextGroupKind::Map,
        "dialogue" => TextGroupKind::EventDialogue,
        "choices" => TextGroupKind::EventChoices,
        "scrolling_text" => TextGroupKind::EventScrollingText,
        "event_command" => TextGroupKind::EventCommand,
        "plugin_parameter" => TextGroupKind::PluginParameter,
        _ => {
            return Err(binding_error(mlua::Error::runtime(format!(
                "translation.prepare kind 无效：{kind_name}"
            ))));
        }
    };
    let Value::String(original) = original else {
        return Err(binding_error(mlua::Error::runtime(format!(
            "translation.prepare original 必须是字符串，实际为 {}",
            original.type_name()
        ))));
    };
    let original =
        lua_string_to_text(&original, "translation.prepare original").map_err(binding_error)?;
    let mut tracker = HostValueBudgetTracker::new(budget);
    tracker
        .container(1)
        .and_then(|()| tracker.string(2, &kind_name))
        .and_then(|()| tracker.string(2, &original))
        .map_err(|error| host_value_budget_error("Translate prepare 参数", error))?;
    Ok((kind, original))
}

fn prepared_translation_to_lua(
    lua: &Lua,
    prepared: Arc<dyn TrustedLuaPreparedTranslation>,
    budget: HostValueBudget,
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

    let native_prepared = Arc::clone(&prepared);
    let native = lua.create_function(move |lua, (_self, candidate): (Value, Value)| {
        let result = parse_translation_candidate(candidate, budget)
            .and_then(|candidate| native_prepared.accept(candidate))
            .and_then(|acceptance| {
                validate_prepared_acceptance_budget(&acceptance, budget)?;
                Ok(acceptance)
            });
        host_result_to_lua(lua, result, prepared_acceptance_to_lua)
    })?;
    table.set("accept", checked_host_function(lua, native)?)?;
    Ok(Value::Table(table))
}

fn parse_translation_candidate(
    candidate: Value,
    budget: HostValueBudget,
) -> Result<String, TrustedLuaHostCallError> {
    let Value::String(candidate) = candidate else {
        return Err(binding_error(mlua::Error::runtime(format!(
            "PreparedText:accept candidate 必须是字符串，实际为 {}",
            candidate.type_name()
        ))));
    };
    let candidate =
        lua_string_to_text(&candidate, "PreparedText:accept candidate").map_err(binding_error)?;
    validate_host_string(&candidate, budget, "PreparedText:accept candidate")?;
    Ok(candidate)
}

fn prepared_acceptance_to_lua(
    lua: &Lua,
    acceptance: TrustedLuaPreparedTranslationAcceptance,
) -> mlua::Result<Value> {
    let table = lua.create_table()?;
    match acceptance {
        TrustedLuaPreparedTranslationAcceptance::Accepted { translation } => {
            table.set("accepted", true)?;
            table.set("translation", translation)?;
        }
        TrustedLuaPreparedTranslationAcceptance::Rejected { reason } => {
            table.set("accepted", false)?;
            table.set("reason", reason)?;
        }
    }
    Ok(Value::Table(table))
}

fn validate_prepared_translation_budget(
    prepared: &dyn TrustedLuaPreparedTranslation,
    budget: HostValueBudget,
) -> Result<(), TrustedLuaHostCallError> {
    let mut tracker = HostValueBudgetTracker::new(budget);
    let result = tracker
        .container(1)
        .and_then(|()| tracker.string(2, prepared.status().as_str()))
        .and_then(|()| tracker.string(2, prepared.model_text()))
        .and_then(|()| tracker.container(2))
        .and_then(|()| {
            prepared.terms().iter().try_for_each(|term| {
                tracker.container(3)?;
                tracker.string(4, term.term())?;
                tracker.string(4, term.translation())
            })
        });
    result.map_err(|error| host_value_budget_error("PreparedText", error))
}

fn validate_prepared_acceptance_budget(
    acceptance: &TrustedLuaPreparedTranslationAcceptance,
    budget: HostValueBudget,
) -> Result<(), TrustedLuaHostCallError> {
    let mut tracker = HostValueBudgetTracker::new(budget);
    let result = tracker.container(1).and_then(|()| {
        tracker.scalar(2)?;
        match acceptance {
            TrustedLuaPreparedTranslationAcceptance::Accepted { translation } => {
                tracker.string(2, translation)
            }
            TrustedLuaPreparedTranslationAcceptance::Rejected { reason } => {
                tracker.string(2, reason)
            }
        }
    });
    result.map_err(|error| host_value_budget_error("PreparedText 验收结果", error))
}

fn install_write_back_context(
    lua: &Lua,
    context: &Table,
    calls: Arc<dyn TrustedLuaWriteBackHostCalls>,
    tokio: Handle,
    cancellation: RuntimeCancellation,
    budget: HostValueBudget,
    markers: &Table,
) -> mlua::Result<()> {
    context.set(
        "output",
        build_output_table(
            lua,
            Arc::clone(&calls),
            tokio,
            cancellation,
            budget,
            markers,
        )?,
    )?;

    let write_back = lua.create_table()?;
    let layout = lua.create_function(move |lua, (region, pairs): (Value, Value)| {
        let result = parse_write_back_layout(region, pairs, budget)
            .and_then(|(region, pairs)| calls.layout(region, pairs))
            .and_then(|layout| {
                validate_write_back_layout_result_budget(&layout, budget)?;
                Ok(layout)
            });
        host_result_to_lua(lua, result, write_back_layout_result_to_lua)
    })?;
    write_back.set("layout", checked_host_function(lua, layout)?)?;
    context.set("write_back", write_back)?;
    Ok(())
}

fn build_output_table(
    lua: &Lua,
    calls: Arc<dyn TrustedLuaWriteBackHostCalls>,
    tokio: Handle,
    cancellation: RuntimeCancellation,
    budget: HostValueBudget,
    markers: &Table,
) -> mlua::Result<Table> {
    let native = lua.create_table()?;

    let read_calls = Arc::clone(&calls);
    let read_tokio = tokio.clone();
    let read_cancellation = cancellation.clone();
    native.set(
        "read",
        lua.create_function(move |lua, path: Value| {
            let result = parse_output_path(path, budget)
                .and_then(|path| {
                    wait_for_output_terminal(
                        &read_tokio,
                        &read_cancellation,
                        read_calls.read_output(path),
                    )
                })
                .and_then(|bytes| {
                    validate_host_bytes(bytes.len(), budget, "写回候选文件")?;
                    Ok(bytes)
                });
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
            let result = parse_output_path(path, budget)
                .and_then(|path| {
                    wait_for_output_terminal(
                        &text_tokio,
                        &text_cancellation,
                        text_calls.read_output(path),
                    )
                })
                .and_then(|bytes| {
                    validate_host_bytes(bytes.len(), budget, "写回候选文本文件")?;
                    Ok(bytes)
                });
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
            let result = parse_output_path(path, budget).and_then(|path| {
                wait_for_output_terminal(
                    &json_tokio,
                    &json_cancellation,
                    json_calls.read_output(path),
                )
            });
            host_result_to_lua(lua, result, |lua, bytes| {
                let text = std::str::from_utf8(&bytes)
                    .map_err(|_| mlua::Error::runtime("写回候选 JSON 文件不是有效 UTF-8"))?;
                let value = decode_json(text, budget).map_err(|error| {
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
            let result = parse_output_path(path, budget)
                .and_then(|path| {
                    wait_for_output_terminal(
                        &list_tokio,
                        &list_cancellation,
                        list_calls.list_output(path),
                    )
                })
                .and_then(|entries| {
                    validate_output_entries_budget(&entries, budget)?;
                    Ok(entries)
                });
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
            let result = parse_output_path(path, budget).and_then(|path| {
                wait_for_output_terminal(
                    &create_tokio,
                    &create_cancellation,
                    create_calls.create_output_directory(path),
                )
            });
            host_result_to_lua(lua, result, |_, ()| Ok(Value::Nil))
        })?,
    )?;

    let write_calls = Arc::clone(&calls);
    let write_tokio = tokio.clone();
    let write_cancellation = cancellation.clone();
    native.set(
        "write",
        lua.create_function(move |lua, (path, bytes): (Value, Value)| {
            let result = parse_output_path(path, budget).and_then(|path| {
                let Value::String(bytes) = bytes else {
                    return Err(binding_error(mlua::Error::runtime(format!(
                        "ctx.output.write bytes 必须是字符串，实际为 {}",
                        bytes.type_name()
                    ))));
                };
                validate_host_bytes(bytes.as_bytes().len(), budget, "写回候选二进制内容")?;
                wait_for_output_terminal(
                    &write_tokio,
                    &write_cancellation,
                    write_calls.write_output(path, bytes.as_bytes().to_vec()),
                )
            });
            host_result_to_lua(lua, result, |_, ()| Ok(Value::Nil))
        })?,
    )?;

    let write_text_calls = Arc::clone(&calls);
    let write_text_tokio = tokio.clone();
    let write_text_cancellation = cancellation.clone();
    native.set(
        "write_text",
        lua.create_function(move |lua, (path, text): (Value, Value)| {
            let result = parse_output_path(path, budget).and_then(|path| {
                let Value::String(text) = text else {
                    return Err(binding_error(mlua::Error::runtime(format!(
                        "ctx.output.write_text text 必须是 UTF-8 字符串，实际为 {}",
                        text.type_name()
                    ))));
                };
                let text = lua_string_to_text(&text, "ctx.output.write_text text")
                    .map_err(binding_error)?;
                validate_host_bytes(text.len(), budget, "写回候选文本内容")?;
                wait_for_output_terminal(
                    &write_text_tokio,
                    &write_text_cancellation,
                    write_text_calls.write_output(path, text.into_bytes()),
                )
            });
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
            let result = parse_output_path(path, budget).and_then(|path| {
                let encoded = JsonEncoder::new(&write_json_markers, budget)
                    .encode(value)
                    .map_err(binding_error)?;
                wait_for_output_terminal(
                    &write_json_tokio,
                    &write_json_cancellation,
                    write_json_calls.write_output(path, encoded.into_bytes()),
                )
            });
            host_result_to_lua(lua, result, |_, ()| Ok(Value::Nil))
        })?,
    )?;

    let remove_calls = calls;
    native.set(
        "remove",
        lua.create_function(move |lua, path: Value| {
            let result = parse_output_path(path, budget).and_then(|path| {
                wait_for_output_terminal(&tokio, &cancellation, remove_calls.remove_output(path))
            });
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

fn parse_output_path(
    value: Value,
    budget: HostValueBudget,
) -> Result<ScopedDirectoryPath, TrustedLuaHostCallError> {
    let Value::String(value) = value else {
        return Err(binding_error(mlua::Error::runtime(format!(
            "候选路径必须是 UTF-8 字符串，实际为 {}",
            value.type_name()
        ))));
    };
    let value = lua_string_to_text(&value, "候选路径").map_err(binding_error)?;
    validate_host_string(&value, budget, "候选路径")?;
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

fn validate_output_entries_budget(
    entries: &[TrustedLuaOutputEntry],
    budget: HostValueBudget,
) -> Result<(), TrustedLuaHostCallError> {
    let mut tracker = HostValueBudgetTracker::new(budget);
    let result = tracker.container(1).and_then(|()| {
        entries.iter().try_for_each(|entry| {
            tracker.container(2)?;
            tracker.string(3, entry.name())?;
            tracker.string(3, entry.kind().as_str())
        })
    });
    result.map_err(|error| host_value_budget_error("候选目录列表", error))
}

fn parse_write_back_layout(
    region: Value,
    pairs: Value,
    budget: HostValueBudget,
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
    let mut tracker = HostValueBudgetTracker::new(budget);
    tracker
        .container(1)
        .and_then(|()| tracker.string(2, &region_name))
        .and_then(|()| tracker.container(2))
        .map_err(|error| host_value_budget_error("写回布局参数", error))?;
    let pairs = dense_values_bounded(pairs, budget, "写回布局参数", binding_error)?
        .into_iter()
        .enumerate()
        .map(|(index, pair)| parse_write_back_layout_pair(index, pair, &mut tracker, 3))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((region, pairs))
}

fn parse_write_back_layout_pair(
    index: usize,
    pair: Value,
    tracker: &mut HostValueBudgetTracker,
    depth: usize,
) -> Result<TrustedLuaWriteBackLayoutPair, TrustedLuaHostCallError> {
    tracker
        .container(depth)
        .map_err(|error| host_value_budget_error("写回布局参数", error))?;
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
    tracker
        .string(depth + 1, &original)
        .map_err(|error| host_value_budget_error("写回布局参数", error))?;
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
    match &translation {
        Some(translation) => tracker.string(depth + 1, translation),
        None => tracker.scalar(depth + 1),
    }
    .map_err(|error| host_value_budget_error("写回布局参数", error))?;
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

fn validate_write_back_layout_result_budget(
    result: &TrustedLuaWriteBackLayoutResult,
    budget: HostValueBudget,
) -> Result<(), TrustedLuaHostCallError> {
    let mut tracker = HostValueBudgetTracker::new(budget);
    let checked = tracker
        .container(1)
        .and_then(|()| tracker.string(2, result.status().as_str()))
        .and_then(|()| tracker.container(2))
        .and_then(|()| {
            result
                .texts()
                .iter()
                .try_for_each(|text| tracker.string(3, text))
        })
        .and_then(|()| tracker.scalar(2))
        .and_then(|()| tracker.scalar(2));
    checked.map_err(|error| host_value_budget_error("写回布局结果", error))
}

fn phase_name(phase: LuaPhase) -> &'static str {
    match phase {
        LuaPhase::Extract => "extract",
        LuaPhase::Translate => "translate",
        LuaPhase::WriteBack => "write_back",
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

fn build_json_table(lua: &Lua, budget: HostValueBudget, markers: &Table) -> mlua::Result<Table> {
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
                json_result_to_lua(lua, result)
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
                if value.len() > budget.max_bytes().get() {
                    return Err(json_budget_error("number 文本", "max_bytes"));
                }
                validate_number(value.as_ref())
                    .map_err(|error| mlua::Error::runtime(format!("ctx.json.number：{error}")))?;
                json_number_to_lua(lua, value.to_owned())
            })();
            json_result_to_lua(lua, result)
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
                let value = decode_json(source.as_ref(), budget)
                    .map_err(|error| mlua::Error::runtime(format!("ctx.json.decode：{error}")))?;
                lossless_json_to_lua(lua, value, &decode_markers)
            })();
            json_result_to_lua(lua, result)
        })?,
    )?;

    let encode_markers = markers.clone();
    native.set(
        "encode",
        lua.create_function(move |lua, value: Value| {
            let result = JsonEncoder::new(&encode_markers, budget)
                .encode(value)
                .and_then(|encoded| lua.create_string(encoded).map(Value::String));
            json_result_to_lua(lua, result)
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
            json_result_to_lua(lua, result)
        })?,
    )?;
    native.set(
        "number_text",
        lua.create_function(move |lua, value: Value| {
            let result = json_number_text(&value)
                .and_then(|text| lua.create_string(text).map(Value::String));
            json_result_to_lua(lua, result)
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

fn json_result_to_lua(lua: &Lua, result: mlua::Result<Value>) -> mlua::Result<MultiValue> {
    host_result_to_lua(lua, result.map_err(json_host_error), |_, value| Ok(value))
}

fn json_host_error(error: mlua::Error) -> TrustedLuaHostCallError {
    TrustedLuaHostCallError::new("json", "invalid_value", error.to_string(), None, None)
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
    match value {
        LosslessJsonValue::Null => Ok(Value::UserData(lua.create_userdata(LuaJsonNull)?)),
        LosslessJsonValue::Boolean(value) => Ok(Value::Boolean(value)),
        LosslessJsonValue::String(value) => Ok(Value::String(lua.create_string(value)?)),
        LosslessJsonValue::Number(value) => json_number_to_lua(lua, value),
        LosslessJsonValue::Array(values) => {
            let table = lua.create_table_with_capacity(values.len(), 0)?;
            mark_json_table(markers, &table, JsonContainerKind::Array)?;
            for (index, value) in values.into_iter().enumerate() {
                table.raw_set(index + 1, lossless_json_to_lua(lua, value, markers)?)?;
            }
            Ok(Value::Table(table))
        }
        LosslessJsonValue::Object(entries) => {
            let table = lua.create_table_with_capacity(0, entries.len())?;
            mark_json_table(markers, &table, JsonContainerKind::Object)?;
            for (key, value) in entries {
                table.raw_set(key, lossless_json_to_lua(lua, value, markers)?)?;
            }
            Ok(Value::Table(table))
        }
    }
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

fn json_budget_error(subject: &str, limit: &str) -> mlua::Error {
    mlua::Error::runtime(format!("JSON {subject} 超过 HostValueBudget.{limit}"))
}

struct JsonEncoder<'a> {
    markers: &'a Table,
    budget: HostValueBudget,
    output: String,
    nodes: usize,
    active_tables: HashSet<usize>,
}

impl<'a> JsonEncoder<'a> {
    fn new(markers: &'a Table, budget: HostValueBudget) -> Self {
        Self {
            markers,
            budget,
            output: String::new(),
            nodes: 0,
            active_tables: HashSet::new(),
        }
    }

    fn encode(mut self, value: Value) -> mlua::Result<String> {
        self.encode_value(value, 1)?;
        Ok(self.output)
    }

    fn encode_value(&mut self, value: Value, depth: usize) -> mlua::Result<()> {
        if depth > self.budget.max_depth().get() {
            return Err(json_budget_error("嵌套深度", "max_depth"));
        }
        self.nodes = self
            .nodes
            .checked_add(1)
            .ok_or_else(|| json_budget_error("节点数", "max_nodes"))?;
        if self.nodes > self.budget.max_nodes().get() {
            return Err(json_budget_error("节点数", "max_nodes"));
        }

        match value {
            Value::Boolean(value) => self.push(if value { "true" } else { "false" }),
            Value::Integer(value) => self.push(value.to_string().as_str()),
            Value::Number(value) => {
                let value = serde_json::Number::from_f64(value)
                    .ok_or_else(|| mlua::Error::runtime("JSON number 不能是 NaN 或 Infinity"))?;
                self.push(value.to_string().as_str())
            }
            Value::String(value) => {
                let value = value
                    .to_str()
                    .map_err(|_| mlua::Error::runtime("JSON string 必须是 UTF-8"))?;
                self.push_json_string(value.as_ref())
            }
            Value::Table(table) => self.encode_table(table, depth),
            Value::UserData(value) if value.is::<LuaJsonNull>() => self.push("null"),
            Value::UserData(value) if value.is::<LuaJsonNumber>() => {
                self.push(value.borrow::<LuaJsonNumber>()?.0.as_str())
            }
            Value::UserData(_) => Err(mlua::Error::runtime(
                "ctx.json.encode 不接受非 JSON userdata",
            )),
            value => Err(mlua::Error::runtime(format!(
                "ctx.json.encode 不接受 {}",
                value.type_name()
            ))),
        }
    }

    fn encode_table(&mut self, table: Table, depth: usize) -> mlua::Result<()> {
        let identity = table.to_pointer() as usize;
        if !self.active_tables.insert(identity) {
            return Err(mlua::Error::runtime("ctx.json.encode 不接受循环 table"));
        }
        let result = match json_table_kind(self.markers, &table)? {
            Some(JsonContainerKind::Array) => self.encode_array(table, depth),
            Some(JsonContainerKind::Object) => self.encode_object(table, depth),
            None => Err(mlua::Error::runtime(
                "ctx.json.encode 的 table 必须先由 ctx.json.array 或 ctx.json.object 显式标记",
            )),
        };
        self.active_tables.remove(&identity);
        result
    }

    fn encode_array(&mut self, table: Table, depth: usize) -> mlua::Result<()> {
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
            count = count
                .checked_add(1)
                .ok_or_else(|| json_budget_error("节点数", "max_nodes"))?;
            if count >= self.budget.max_nodes().get() {
                return Err(json_budget_error("节点数", "max_nodes"));
            }
            maximum = maximum.max(key);
        }
        if count != maximum {
            return Err(mlua::Error::runtime("JSON array 不允许洞"));
        }

        self.push("[")?;
        for index in 1..=maximum {
            if index != 1 {
                self.push(",")?;
            }
            self.encode_value(table.raw_get::<Value>(index)?, depth + 1)?;
        }
        self.push("]")
    }

    fn encode_object(&mut self, table: Table, depth: usize) -> mlua::Result<()> {
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
            if entries.len() >= self.budget.max_nodes().get() {
                return Err(json_budget_error("节点数", "max_nodes"));
            }
            entries.push((key, value));
        }
        entries.sort_by(|left, right| left.0.cmp(&right.0));

        self.push("{")?;
        for (index, (key, value)) in entries.into_iter().enumerate() {
            if index != 0 {
                self.push(",")?;
            }
            self.push_json_string(&key)?;
            self.push(":")?;
            self.encode_value(value, depth + 1)?;
        }
        self.push("}")
    }

    fn push_json_string(&mut self, value: &str) -> mlua::Result<()> {
        let encoded =
            serde_json::to_string(value).expect("有效 UTF-8 字符串必须可以序列化为 JSON string");
        self.push(&encoded)
    }

    fn push(&mut self, value: &str) -> mlua::Result<()> {
        let length = self
            .output
            .len()
            .checked_add(value.len())
            .ok_or_else(|| json_budget_error("输出", "max_bytes"))?;
        if length > self.budget.max_bytes().get() {
            return Err(json_budget_error("输出", "max_bytes"));
        }
        self.output.push_str(value);
        Ok(())
    }
}

fn build_source_table(
    lua: &Lua,
    calls: Arc<dyn TrustedLuaCommonHostCalls>,
    tokio: Handle,
    cancellation: RuntimeCancellation,
    budget: HostValueBudget,
    markers: &Table,
) -> mlua::Result<Table> {
    let native = lua.create_table()?;

    let read_calls = Arc::clone(&calls);
    let read_tokio = tokio.clone();
    let read_cancellation = cancellation.clone();
    native.set(
        "read",
        lua.create_function(move |lua, path: Value| {
            let result = parse_source_path(path, budget)
                .and_then(|path| {
                    wait_for_host(
                        &read_tokio,
                        &read_cancellation,
                        read_calls.read_source(path),
                    )
                })
                .and_then(|bytes| {
                    validate_host_bytes(bytes.len(), budget, "来源文件")?;
                    Ok(bytes)
                });
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
            let result = parse_source_path(path, budget)
                .and_then(|path| {
                    wait_for_host(
                        &text_tokio,
                        &text_cancellation,
                        text_calls.read_source(path),
                    )
                })
                .and_then(|bytes| {
                    validate_host_bytes(bytes.len(), budget, "来源文本文件")?;
                    Ok(bytes)
                });
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
            let result = parse_source_path(path, budget).and_then(|path| {
                wait_for_host(
                    &json_tokio,
                    &json_cancellation,
                    json_calls.read_source(path),
                )
            });
            host_result_to_lua(lua, result, |lua, bytes| {
                let text = std::str::from_utf8(&bytes)
                    .map_err(|_| mlua::Error::runtime("来源 JSON 文件不是有效 UTF-8"))?;
                let value = decode_json(text, budget)
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
            let result = parse_source_path(path, budget)
                .and_then(|path| wait_for_host(&tokio, &cancellation, list_calls.list_source(path)))
                .and_then(|entries| {
                    validate_string_array_budget(&entries, budget, "来源目录列表")?;
                    Ok(entries)
                });
            host_result_to_lua(lua, result, |lua, entries| {
                let values = entries.into_iter().map(LosslessJsonValue::String).collect();
                lossless_json_to_lua(lua, LosslessJsonValue::Array(values), &list_markers)
            })
        })?,
    )?;

    checked_function_table(lua, native, &["read", "read_text", "read_json", "list"])
}

fn build_mz_table(
    lua: &Lua,
    calls: Arc<dyn TrustedLuaCommonHostCalls>,
    tokio: Handle,
    cancellation: RuntimeCancellation,
    budget: HostValueBudget,
    markers: &Table,
) -> mlua::Result<Table> {
    let native = lua.create_table()?;
    native.set(
        "data",
        lua.create_function(move |lua, file_name: Value| {
            let result = parse_mz_string(file_name, "MZ Data 文件名")
                .and_then(|file_name| {
                    validate_host_string(&file_name, budget, "MZ Data 文件名")?;
                    Ok(file_name)
                })
                .and_then(|file_name| data_source(&file_name).map_err(mz_host_error));
            host_result_to_lua(lua, result, |lua, source| {
                lua.create_userdata(LuaMzSource(source))
                    .map(Value::UserData)
            })
        })?,
    )?;
    native.set(
        "map",
        lua.create_function(|lua, map_id: Value| {
            let result = parse_mz_integer(map_id, "MZ map ID")
                .and_then(|map_id| map_source(map_id).map_err(mz_host_error));
            host_result_to_lua(lua, result, |lua, source| {
                lua.create_userdata(LuaMzSource(source))
                    .map(Value::UserData)
            })
        })?,
    )?;
    native.set(
        "plugin_parameter",
        lua.create_function(
            move |lua, (plugin_index, plugin_name, parameter_name): (Value, Value, Value)| {
                let result = parse_mz_integer(plugin_index, "插件索引").and_then(|plugin_index| {
                    parse_mz_string(plugin_name, "插件名").and_then(|plugin_name| {
                        parse_mz_string(parameter_name, "插件参数名").and_then(|parameter_name| {
                            validate_mz_plugin_source_budget(
                                plugin_index,
                                &plugin_name,
                                &parameter_name,
                                budget,
                            )?;
                            plugin_parameter_source(plugin_index, &plugin_name, &parameter_name)
                                .map_err(mz_host_error)
                        })
                    })
                });
                host_result_to_lua(lua, result, |lua, source| {
                    lua.create_userdata(LuaMzSource(source))
                        .map(Value::UserData)
                })
            },
        )?,
    )?;

    let open_markers = markers.clone();
    native.set(
        "open",
        lua.create_function(move |lua, source: Value| {
            let result = parse_mz_source(source)
                .and_then(|source| {
                    let path = source_path(&source);
                    wait_for_host(&tokio, &cancellation, calls.read_source(path))
                        .map(|bytes| (source, bytes))
                })
                .and_then(|(source, bytes)| {
                    OpenedMzDocument::open(source, &bytes, budget).map_err(mz_host_error)
                });
            host_result_to_lua(lua, result, |lua, document| {
                lua.create_userdata(LuaMzDocument {
                    document,
                    markers: open_markers.clone(),
                    budget,
                })
                .map(Value::UserData)
            })
        })?,
    )?;
    let table = checked_function_table(lua, native, &["data", "map", "plugin_parameter", "open"])?;
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

fn parse_source_path(
    value: Value,
    budget: HostValueBudget,
) -> Result<LuaSourcePath, TrustedLuaHostCallError> {
    let Value::String(value) = value else {
        return Err(binding_error(mlua::Error::runtime(format!(
            "来源路径必须是 UTF-8 字符串，实际为 {}",
            value.type_name()
        ))));
    };
    let value = lua_string_to_text(&value, "来源路径").map_err(binding_error)?;
    validate_host_string(&value, budget, "来源路径")?;
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

fn validate_string_array_budget(
    entries: &[String],
    budget: HostValueBudget,
    subject: &str,
) -> Result<(), TrustedLuaHostCallError> {
    let mut tracker = HostValueBudgetTracker::new(budget);
    tracker
        .container(1)
        .and_then(|()| {
            entries
                .iter()
                .try_for_each(|entry| tracker.string(2, entry))
        })
        .map_err(|error| host_value_budget_error(subject, error))
}

fn validate_host_bytes(
    actual: usize,
    budget: HostValueBudget,
    subject: &str,
) -> Result<(), TrustedLuaHostCallError> {
    let mut tracker = HostValueBudgetTracker::new(budget);
    tracker
        .binary_len(1, actual)
        .map_err(|error| host_value_budget_error(subject, error))
}

fn validate_host_string(
    value: &str,
    budget: HostValueBudget,
    subject: &str,
) -> Result<(), TrustedLuaHostCallError> {
    let mut tracker = HostValueBudgetTracker::new(budget);
    tracker
        .string(1, value)
        .map_err(|error| host_value_budget_error(subject, error))
}

fn track_mz_text_reference(
    tracker: &mut HostValueBudgetTracker,
    depth: usize,
    reference: &MzTextReference,
) -> Result<(), HostValueBudgetExceeded> {
    tracker.container(depth)?;
    tracker.string(depth + 1, reference.original())?;
    track_mz_location(tracker, depth + 1, reference.location())
}

fn track_mz_location(
    tracker: &mut HostValueBudgetTracker,
    depth: usize,
    location: &MzLocation,
) -> Result<(), HostValueBudgetExceeded> {
    tracker.container(depth)?;
    track_mz_source(tracker, depth + 1, location.source())?;
    tracker.container(depth + 1)?;
    for step in location.steps() {
        match step {
            MzLocationStep::ObjectKey(key) => tracker.string(depth + 2, key)?,
            MzLocationStep::ArrayIndex(_) | MzLocationStep::DecodeJsonString => {
                tracker.scalar(depth + 2)?;
            }
        }
    }
    match location {
        MzLocation::Value { .. } => Ok(()),
        MzLocation::NoteTag { tag_name, .. } | MzLocation::CommentTag { tag_name, .. } => {
            tracker.string(depth + 1, tag_name)?;
            tracker.scalar(depth + 1)
        }
    }
}

fn track_mz_source(
    tracker: &mut HostValueBudgetTracker,
    depth: usize,
    source: &MzSource,
) -> Result<(), HostValueBudgetExceeded> {
    match source {
        MzSource::Data(_) | MzSource::Map(_) => tracker.scalar(depth),
        MzSource::PluginParameter {
            plugin_name,
            parameter_name,
            ..
        } => {
            tracker.container(depth)?;
            tracker.scalar(depth + 1)?;
            tracker.string(depth + 1, plugin_name)?;
            tracker.string(depth + 1, parameter_name)
        }
    }
}

fn parse_mz_string(value: Value, role: &str) -> Result<String, TrustedLuaHostCallError> {
    let Value::String(value) = value else {
        return Err(mz_argument_error(format!(
            "{role} 必须是 UTF-8 字符串，实际为 {}",
            value.type_name()
        )));
    };
    lua_string_to_text(&value, role).map_err(|error| mz_argument_error(error.to_string()))
}

fn parse_mz_integer(value: Value, role: &str) -> Result<i64, TrustedLuaHostCallError> {
    let Value::Integer(value) = value else {
        return Err(mz_argument_error(format!(
            "{role} 必须是 Lua integer，实际为 {}",
            value.type_name()
        )));
    };
    Ok(value)
}

fn parse_mz_source(value: Value) -> Result<MzSource, TrustedLuaHostCallError> {
    let Value::UserData(value) = value else {
        return Err(mz_argument_error(format!(
            "ctx.mz.open 只接受 Rust 建立的 MzSource，实际为 {}",
            value.type_name()
        )));
    };
    if !value.is::<LuaMzSource>() {
        return Err(mz_argument_error(
            "ctx.mz.open 只接受 Rust 建立的 MzSource".to_owned(),
        ));
    }
    value
        .borrow::<LuaMzSource>()
        .map(|source| source.0.clone())
        .map_err(|error| mz_argument_error(error.to_string()))
}

fn mz_argument_error(message: String) -> TrustedLuaHostCallError {
    TrustedLuaHostCallError::new("mz", "invalid_argument", message, None, None)
}

fn mz_host_error(error: MzDocumentError) -> TrustedLuaHostCallError {
    let kind = error.kind();
    let message = error.to_string();
    TrustedLuaHostCallError::new("mz", kind, message, None, Some(Arc::new(error)))
}

fn validate_mz_plugin_source_budget(
    _plugin_index: i64,
    plugin_name: &str,
    parameter_name: &str,
    budget: HostValueBudget,
) -> Result<(), TrustedLuaHostCallError> {
    let mut tracker = HostValueBudgetTracker::new(budget);
    tracker
        .container(1)
        .and_then(|()| tracker.scalar(2))
        .and_then(|()| tracker.string(2, plugin_name))
        .and_then(|()| tracker.string(2, parameter_name))
        .map_err(|error| host_value_budget_error("MZ 插件参数来源", error))
}

fn build_project_table(lua: &Lua, project: &LuaProjectContext) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("name", project.name().as_str())?;
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
    budget: HostValueBudget,
) -> mlua::Result<Table> {
    let native = lua.create_table()?;
    let null = lua.create_userdata(LuaSqliteNull)?;

    let query_calls = Arc::clone(&calls);
    let query_tokio = tokio.clone();
    let query_cancellation = cancellation.clone();
    native.set(
        "query",
        lua.create_function(move |lua, (statement, parameters): (Value, Value)| {
            let result = parse_sql_call(statement, parameters, budget)
                .and_then(|(statement, parameters)| {
                    wait_for_host(
                        &query_tokio,
                        &query_cancellation,
                        query_calls.query(SqliteQuery::new(statement, parameters)),
                    )
                })
                .and_then(|rows| {
                    validate_sqlite_rows_budget(&rows, budget)?;
                    Ok(rows)
                });
            host_result_to_lua(lua, result, rows_to_lua)
        })?,
    )?;

    let execute_calls = Arc::clone(&calls);
    let execute_tokio = tokio.clone();
    let execute_cancellation = cancellation.clone();
    native.set(
        "execute",
        lua.create_function(move |lua, (statement, parameters): (Value, Value)| {
            let result = parse_sql_call(statement, parameters, budget).and_then(
                |(statement, parameters)| {
                    wait_for_host(
                        &execute_tokio,
                        &execute_cancellation,
                        execute_calls.execute(SqliteCommand::new(statement, parameters)),
                    )
                },
            );
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
                    wait_for_host(&tokio, &cancellation, future),
                    |_, ()| Ok(Value::Nil),
                )
            })?,
        )?;
    }

    let blob = lua.create_function(move |lua, bytes: Value| {
        let result = match bytes {
            Value::String(bytes) => {
                validate_host_bytes(bytes.as_bytes().len(), budget, "SQLite BLOB")
                    .map(|()| bytes.as_bytes().to_vec())
            }
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
    budget: HostValueBudget,
) -> mlua::Result<Function> {
    let native = lua.create_function(move |lua, messages: Value| {
        let result = parse_message_array(messages, budget)
            .and_then(|messages| wait_for_host(&tokio, &cancellation, calls.request_llm(messages)))
            .and_then(|response| {
                validate_llm_response_budget(&response, budget)?;
                Ok(response)
            });
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
    TrustedLuaHostCallError::new("binding", "invalid_value", error.to_string(), None, None)
}

fn host_value_budget_error(
    subject: &str,
    error: HostValueBudgetExceeded,
) -> TrustedLuaHostCallError {
    TrustedLuaHostCallError::new(
        "binding",
        "host_value_budget_exceeded",
        format!("{subject}：{error}"),
        None,
        Some(Arc::new(error)),
    )
}

fn parse_sql_call(
    statement: Value,
    parameters: Value,
    budget: HostValueBudget,
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
    let mut tracker = HostValueBudgetTracker::new(budget);
    tracker
        .container(1)
        .and_then(|()| tracker.string(2, &statement))
        .map_err(|error| host_value_budget_error("SQLite 调用参数", error))?;
    let parameters = parse_parameters(parameters, &mut tracker, 2, budget)?;
    Ok((statement, parameters))
}

fn parse_parameters(
    value: Value,
    tracker: &mut HostValueBudgetTracker,
    depth: usize,
    budget: HostValueBudget,
) -> Result<Vec<SqliteValue>, TrustedLuaHostCallError> {
    match value {
        Value::Nil => {
            tracker
                .scalar(depth)
                .map_err(|error| host_value_budget_error("SQLite 调用参数", error))?;
            Ok(Vec::new())
        }
        Value::Table(table) => {
            tracker
                .container(depth)
                .map_err(|error| host_value_budget_error("SQLite 调用参数", error))?;
            dense_values_bounded(table, budget, "SQLite 调用参数", binding_error)?
                .into_iter()
                .map(|value| {
                    let value = lua_to_sqlite_value(value).map_err(binding_error)?;
                    track_sqlite_value(tracker, depth + 1, &value)
                        .map_err(|error| host_value_budget_error("SQLite 调用参数", error))?;
                    Ok(value)
                })
                .collect()
        }
        other => Err(binding_error(mlua::Error::runtime(format!(
            "SQLite parameters 必须是无洞数组或 nil，实际为 {}",
            other.type_name()
        )))),
    }
}

fn dense_values(table: Table) -> mlua::Result<Vec<Value>> {
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

fn dense_values_bounded<F>(
    table: Table,
    budget: HostValueBudget,
    subject: &str,
    invalid: F,
) -> Result<Vec<Value>, TrustedLuaHostCallError>
where
    F: FnOnce(mlua::Error) -> TrustedLuaHostCallError,
{
    let mut tracker = HostValueBudgetTracker::new(budget);
    tracker
        .container(1)
        .map_err(|error| host_value_budget_error(subject, error))?;
    for pair in table.clone().pairs::<Value, Value>() {
        let _ = pair.map_err(binding_error)?;
        tracker
            .scalar(2)
            .map_err(|error| host_value_budget_error(subject, error))?;
    }
    dense_values(table).map_err(invalid)
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

fn validate_sqlite_rows_budget(
    rows: &[SqliteRow],
    budget: HostValueBudget,
) -> Result<(), TrustedLuaHostCallError> {
    let mut tracker = HostValueBudgetTracker::new(budget);
    let result = tracker.container(1).and_then(|()| {
        rows.iter().try_for_each(|row| {
            tracker.container(2)?;
            row.values()
                .iter()
                .try_for_each(|value| track_sqlite_value(&mut tracker, 3, value))
        })
    });
    result.map_err(|error| host_value_budget_error("SQLite 查询结果", error))
}

fn track_sqlite_value(
    tracker: &mut HostValueBudgetTracker,
    depth: usize,
    value: &SqliteValue,
) -> Result<(), HostValueBudgetExceeded> {
    match value {
        SqliteValue::Null | SqliteValue::Integer(_) | SqliteValue::Real(_) => tracker.scalar(depth),
        SqliteValue::Text(value) => tracker.string(depth, value),
        SqliteValue::Blob(value) => tracker.binary(depth, value),
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

fn parse_messages(
    table: Table,
    tracker: &mut HostValueBudgetTracker,
    budget: HostValueBudget,
) -> Result<Vec<ChatMessage>, TrustedLuaHostCallError> {
    dense_values_bounded(table, budget, "LLM messages", binding_error)?
        .into_iter()
        .map(|value| {
            tracker
                .container(2)
                .map_err(|error| host_value_budget_error("LLM messages", error))?;
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
            tracker
                .string(3, &role)
                .and_then(|()| tracker.string(3, &content))
                .map_err(|error| host_value_budget_error("LLM messages", error))?;
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

fn parse_message_array(
    value: Value,
    budget: HostValueBudget,
) -> Result<Vec<ChatMessage>, TrustedLuaHostCallError> {
    let Value::Table(messages) = value else {
        return Err(binding_error(mlua::Error::runtime(format!(
            "LLM messages 必须是无洞数组，实际为 {}",
            value.type_name()
        ))));
    };
    let mut tracker = HostValueBudgetTracker::new(budget);
    tracker
        .container(1)
        .map_err(|error| host_value_budget_error("LLM messages", error))?;
    parse_messages(messages, &mut tracker, budget)
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

fn validate_llm_response_budget(
    response: &LlmResponse,
    budget: HostValueBudget,
) -> Result<(), TrustedLuaHostCallError> {
    let mut tracker = HostValueBudgetTracker::new(budget);
    let result = tracker
        .container(1)
        .and_then(|()| tracker.string(2, response.content()))
        .and_then(|()| tracker.string(2, &response.finish_reason().to_string()))
        .and_then(|()| match response.provider_request_id() {
            Some(request_id) => tracker.string(2, request_id),
            None => tracker.scalar(2),
        })
        .and_then(|()| match response.provider_response_id() {
            Some(response_id) => tracker.string(2, response_id),
            None => tracker.scalar(2),
        })
        .and_then(|()| match response.usage() {
            Some(_) => {
                tracker.container(2)?;
                tracker.scalar(3)?;
                tracker.scalar(3)?;
                tracker.scalar(3)
            }
            None => tracker.scalar(2),
        });
    result.map_err(|error| host_value_budget_error("LLM 响应", error))
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
struct LuaMzSource(MzSource);

impl UserData for LuaMzSource {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::Eq, |_lua, this, other: AnyUserData| {
            if !other.is::<LuaMzSource>() {
                return Ok(false);
            }
            Ok(other.borrow::<LuaMzSource>()?.0 == this.0)
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
struct LuaMzDocument {
    document: OpenedMzDocument,
    markers: Table,
    budget: HostValueBudget,
}

impl UserData for LuaMzDocument {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("value", |lua, this| {
            let document = this.document.clone();
            let markers = this.markers.clone();
            let budget = this.budget;
            let native = lua.create_function(move |lua, (_document, path): (Value, Value)| {
                let result = parse_mz_path(path, budget)
                    .and_then(|steps| document.value(&steps).map_err(mz_host_error));
                host_result_to_lua(lua, result, |lua, value| {
                    lossless_json_to_lua(lua, value, &markers)
                })
            })?;
            checked_host_function(lua, native)
        });
        fields.add_field_method_get("location", |lua, this| {
            let document = this.document.clone();
            let budget = this.budget;
            let native = lua.create_function(move |lua, (_document, path): (Value, Value)| {
                let result = parse_mz_path(path, budget)
                    .and_then(|steps| document.location(&steps).map_err(mz_host_error));
                host_result_to_lua(lua, result, |lua, location| {
                    lua.create_userdata(LuaMzLocation(location))
                        .map(Value::UserData)
                })
            })?;
            checked_host_function(lua, native)
        });
        fields.add_field_method_get("text", |lua, this| {
            let document = this.document.clone();
            let budget = this.budget;
            let native = lua.create_function(move |lua, (_document, path): (Value, Value)| {
                let result = parse_mz_path(path, budget)
                    .and_then(|steps| document.text(&steps).map_err(mz_host_error));
                host_result_to_lua(lua, result, |lua, reference| {
                    lua.create_userdata(LuaMzTextReference(reference))
                        .map(Value::UserData)
                })
            })?;
            checked_host_function(lua, native)
        });
        fields.add_field_method_get("note_tag", |lua, this| {
            let document = this.document.clone();
            let budget = this.budget;
            let native = lua.create_function(
                move |lua,
                      (_document, path, tag_name, occurrence): (
                    Value,
                    Value,
                    Value,
                    Value,
                )| {
                    let result = parse_mz_path(path, budget).and_then(|steps| {
                        parse_mz_string(tag_name, "Note 标签名").and_then(|tag_name| {
                            validate_host_string(&tag_name, budget, "Note 标签名")?;
                            parse_mz_occurrence(occurrence).and_then(|occurrence| {
                                document
                                    .note_tag(&steps, &tag_name, occurrence)
                                    .map_err(mz_host_error)
                            })
                        })
                    });
                    host_result_to_lua(lua, result, |lua, reference| {
                        lua.create_userdata(LuaMzTextReference(reference))
                            .map(Value::UserData)
                    })
                },
            )?;
            checked_host_function(lua, native)
        });
        fields.add_field_method_get("comment_tag", |lua, this| {
            let document = this.document.clone();
            let budget = this.budget;
            let native = lua.create_function(
                move |lua,
                      (_document, path, tag_name, occurrence): (
                    Value,
                    Value,
                    Value,
                    Value,
                )| {
                    let result = parse_mz_path(path, budget).and_then(|steps| {
                        parse_mz_string(tag_name, "Comment 标签名").and_then(|tag_name| {
                            validate_host_string(&tag_name, budget, "Comment 标签名")?;
                            parse_mz_occurrence(occurrence).and_then(|occurrence| {
                                document
                                    .comment_tag(&steps, &tag_name, occurrence)
                                    .map_err(mz_host_error)
                            })
                        })
                    });
                    host_result_to_lua(lua, result, |lua, reference| {
                        lua.create_userdata(LuaMzTextReference(reference))
                            .map(Value::UserData)
                    })
                },
            )?;
            checked_host_function(lua, native)
        });
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::ToString, |_lua, this, ()| {
            Ok(format!("MzDocument({})", this.document.source()))
        });
    }
}

#[derive(Clone, Debug)]
struct LuaMzLocation(MzLocation);

impl UserData for LuaMzLocation {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::Eq, |_lua, this, other: AnyUserData| {
            if !other.is::<LuaMzLocation>() {
                return Ok(false);
            }
            Ok(other.borrow::<LuaMzLocation>()?.0 == this.0)
        });
        methods.add_meta_method(MetaMethod::ToString, |_lua, this, ()| {
            Ok(this.0.to_string())
        });
    }
}

#[derive(Clone, Debug)]
struct LuaMzTextReference(MzTextReference);

impl UserData for LuaMzTextReference {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("original", |_lua, this| Ok(this.0.original().to_owned()));
        fields.add_field_method_get("location", |lua, this| {
            lua.create_userdata(LuaMzLocation(this.0.location().clone()))
        });
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::ToString, |_lua, this, ()| {
            Ok(this.0.original().to_owned())
        });
    }
}

fn parse_mz_path(
    path: Value,
    budget: HostValueBudget,
) -> Result<Vec<MzLocationStep>, TrustedLuaHostCallError> {
    let Value::Table(path) = path else {
        return Err(mz_argument_error(format!(
            "MZ path 必须是无洞数组，实际为 {}",
            path.type_name()
        )));
    };
    let mut tracker = HostValueBudgetTracker::new(budget);
    tracker
        .container(1)
        .map_err(|error| host_value_budget_error("MZ path", error))?;
    dense_values_bounded(path, budget, "MZ path", |error| {
        mz_argument_error(error.to_string())
    })?
    .into_iter()
    .map(|value| match value {
        Value::String(key) => {
            let key = lua_string_to_text(&key, "MZ object key")
                .map_err(|error| mz_argument_error(error.to_string()))?;
            tracker
                .string(2, &key)
                .map_err(|error| host_value_budget_error("MZ path", error))?;
            Ok(MzLocationStep::key(key))
        }
        Value::Integer(index) => {
            tracker
                .scalar(2)
                .map_err(|error| host_value_budget_error("MZ path", error))?;
            usize::try_from(index)
                .map(MzLocationStep::index)
                .map_err(|_| mz_argument_error("MZ array index 必须是非负 Lua integer".to_owned()))
        }
        Value::UserData(value) if value.is::<LuaDecodeJsonString>() => {
            tracker
                .scalar(2)
                .map_err(|error| host_value_budget_error("MZ path", error))?;
            Ok(MzLocationStep::DecodeJsonString)
        }
        value => Err(mz_argument_error(format!(
            "MZ path 只接受字符串、非负整数或 ctx.mz.DECODE_JSON，实际为 {}",
            value.type_name()
        ))),
    })
    .collect()
}

fn parse_mz_occurrence(value: Value) -> Result<usize, TrustedLuaHostCallError> {
    let value = parse_mz_integer(value, "MZ 标签 occurrence")?;
    usize::try_from(value)
        .map_err(|_| mz_argument_error("MZ 标签 occurrence 必须是非负 Lua integer".to_owned()))
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

fn vm_error(context: &str, error: mlua::Error, max_error_bytes: usize) -> TrustedLua54RuntimeError {
    TrustedLua54RuntimeError::Vm(truncate_utf8(
        format!("{context}：{error}"),
        max_error_bytes,
    ))
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

fn truncate_utf8(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let mut boundary = max_bytes;
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::Duration;

    use crate::att_mz::ProjectName;
    use crate::att_mz::lua::runtime::{
        TrustedLuaBindingFinalization, TrustedLuaBindingFinalizer, TrustedLuaExtractIntent,
        TrustedLuaPreparedTranslationStatus, TrustedLuaRuntimeBindings, TrustedLuaTranslationTerm,
    };
    use crate::att_mz::project::OpenedProject;

    #[derive(Default)]
    struct TestObservations {
        executed_parameters: Vec<SqliteValue>,
        messages: Vec<ChatMessage>,
        prepared: Vec<(TextGroupKind, String)>,
        extract_intents: Vec<TrustedLuaExtractIntent>,
        output_operations: Vec<String>,
        output_writes: Vec<(String, Vec<u8>)>,
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
                        r#"{"list":[{"code":108,"parameters":["<Quest:第一"]},{"code":408,"parameters":["行>"]},{"code":0,"parameters":[]}]}"#
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
        ) -> Result<Arc<dyn TrustedLuaPreparedTranslation>, TrustedLuaHostCallError> {
            self.observations
                .lock()
                .expect("测试记录锁不应中毒")
                .prepared
                .push((kind, original.clone()));
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
                _ => Ok(TrustedLuaPreparedTranslationAcceptance::accepted(format!(
                    "{candidate}\\C[2]"
                ))),
            }
        }
    }

    impl TrustedLuaExtractHostCalls for TestCalls {
        fn replace_standard(&self, snapshot: LuaSnapshot) -> Result<(), TrustedLuaHostCallError> {
            self.observations
                .lock()
                .expect("测试观察锁不应中毒")
                .extract_intents
                .push(TrustedLuaExtractIntent::Replace(snapshot));
            Ok(())
        }

        fn clear_standard(&self) -> Result<(), TrustedLuaHostCallError> {
            self.observations
                .lock()
                .expect("测试观察锁不应中毒")
                .extract_intents
                .push(TrustedLuaExtractIntent::Deactivate);
            Ok(())
        }
    }

    impl TrustedLuaWriteBackHostCalls for TestCalls {
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
                        crate::att_mz::lua::runtime::TrustedLuaOutputEntryKind::Directory,
                    ),
                    TrustedLuaOutputEntry::new(
                        "text.txt".to_owned(),
                        crate::att_mz::lua::runtime::TrustedLuaOutputEntryKind::File,
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
                crate::att_mz::lua::runtime::TrustedLuaWriteBackLayoutStatus::Applied,
                pairs
                    .iter()
                    .map(|pair| pair.translation().unwrap_or(pair.original()).to_owned())
                    .collect(),
                1,
                2,
            ))
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
        test_configuration_with_budget(HostValueBudget::new(
            NonZeroUsize::new(1024 * 1024).unwrap(),
            NonZeroUsize::new(10_000).unwrap(),
            NonZeroUsize::new(64).unwrap(),
        ))
    }

    fn test_configuration_with_budget(
        host_value_budget: HostValueBudget,
    ) -> TrustedLua54RuntimeConfiguration {
        TrustedLua54RuntimeConfiguration::new(
            NonZeroUsize::new(2 * 1024 * 1024).unwrap(),
            NonZeroUsize::new(16 * 1024 * 1024).unwrap(),
            NonZeroU32::new(100).unwrap(),
            NonZeroUsize::new(4096).unwrap(),
            host_value_budget,
        )
    }

    fn assert_host_budget_error<T>(result: Result<T, TrustedLuaHostCallError>) {
        let error = match result {
            Ok(_) => panic!("值应超过统一 HostValueBudget"),
            Err(error) => error,
        };
        assert_eq!(error.domain(), "binding");
        assert_eq!(error.kind(), "host_value_budget_exceeded");
    }

    fn test_project() -> LuaProjectContext {
        LuaProjectContext::from_opened_project(&OpenedProject::new(
            "demo".parse::<ProjectName>().unwrap(),
            PathBuf::from(r"C:\projects\demo"),
            PathBuf::from(r"C:\projects\demo\project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            crate::att_mz::project::test_layout_profile(),
        ))
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

    #[test]
    fn dense_arrays_reject_holes_and_map_keys() {
        let lua = Lua::new();
        let hole: Table = lua.load("return {[1] = 'a', [3] = 'c'}").eval().unwrap();
        assert!(dense_values(hole).is_err());
        let map: Table = lua.load("return {name = 'alice'}").eval().unwrap();
        assert!(dense_values(map).is_err());
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
        let budget = HostValueBudget::new(
            NonZeroUsize::new(1024).unwrap(),
            NonZeroUsize::new(32).unwrap(),
            NonZeroUsize::new(4).unwrap(),
        );
        let mut tracker = HostValueBudgetTracker::new(budget);
        assert_eq!(
            parse_parameters(Value::Table(values), &mut tracker, 1, budget).unwrap(),
            vec![
                SqliteValue::Text("text".to_owned()),
                SqliteValue::Blob(vec![0, 255]),
                SqliteValue::Null,
            ]
        );
    }

    #[test]
    fn every_core_value_boundary_uses_the_same_host_value_budget() {
        let lua = Lua::new();
        let budget = HostValueBudget::new(
            NonZeroUsize::new(8).unwrap(),
            NonZeroUsize::new(64).unwrap(),
            NonZeroUsize::new(8).unwrap(),
        );

        let groups: Table = lua
            .load("return {{kind = '123456789', location = {}, fields = {}}}")
            .eval()
            .unwrap();
        assert_host_budget_error(parse_lua_standard_snapshot(Value::Table(groups), budget));

        let empty = lua.create_table().unwrap();
        assert_host_budget_error(parse_write_back_layout(
            Value::String(lua.create_string("dialogue_body").unwrap()),
            Value::Table(empty),
            budget,
        ));
        assert_host_budget_error(parse_source_path(
            Value::String(lua.create_string("data/123456789").unwrap()),
            budget,
        ));
        assert_host_budget_error(parse_output_path(
            Value::String(lua.create_string("data/123456789").unwrap()),
            budget,
        ));

        let messages: Table = lua
            .load("return {{role = 'user', content = '12345'}}")
            .eval()
            .unwrap();
        assert_host_budget_error(parse_message_array(Value::Table(messages), budget));

        let parameters = lua.create_table().unwrap();
        parameters.raw_set(1, "12345678").unwrap();
        assert_host_budget_error(parse_sql_call(
            Value::String(lua.create_string("q").unwrap()),
            Value::Table(parameters),
            budget,
        ));
        assert_host_budget_error(validate_sqlite_rows_budget(
            &[SqliteRow::new(vec![SqliteValue::Text(
                "123456789".to_owned(),
            )])],
            budget,
        ));

        let response = LlmResponse::new(
            "123456789",
            crate::llm::LlmFinishReason::Stop,
            None,
            Some("r".to_owned()),
            None,
        );
        assert_host_budget_error(validate_llm_response_budget(&response, budget));

        let path: Table = lua.load("return {'123456789'}").eval().unwrap();
        assert_host_budget_error(parse_mz_path(Value::Table(path), budget));

        assert_host_budget_error(validate_write_back_layout_result_budget(
            &TrustedLuaWriteBackLayoutResult::new(
                crate::att_mz::lua::runtime::TrustedLuaWriteBackLayoutStatus::Applied,
                vec!["123456789".to_owned()],
                0,
                0,
            ),
            budget,
        ));
        assert_host_budget_error(validate_prepared_translation_budget(
            &TestPreparedTranslation {
                status: TrustedLuaPreparedTranslationStatus::Active,
                model_text: "123456789".to_owned(),
                terms: Vec::new(),
            },
            budget,
        ));
        assert_host_budget_error(validate_prepared_acceptance_budget(
            &TrustedLuaPreparedTranslationAcceptance::accepted("123456789"),
            budget,
        ));
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
    fn error_truncation_preserves_utf8() {
        assert_eq!(truncate_utf8("中文abc".to_owned(), 4), "中");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn extract_replace_standard_builds_a_valid_unforgeable_snapshot() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());
        let observations = Arc::new(Mutex::new(TestObservations::default()));
        run_extract_program(
            &runtime,
            Arc::clone(&observations),
            r#"
local items = ctx.mz.open(ctx.mz.data("Items.json"))
ctx.extract.replace_standard({
  {
    kind = "database_entry",
    location = items:location({1}),
    fields = {
      { name = "name", text = items:text({1, "name"}) },
    },
  },
})
"#,
        )
        .await
        .expect("合法 MZ 引用应该建立 Lua 标准快照");

        {
            let observations = observations.lock().expect("测试观察锁不应中毒");
            let [TrustedLuaExtractIntent::Replace(snapshot)] =
                observations.extract_intents.as_slice()
            else {
                panic!("Runtime 应只记录一次 Replace 意图")
            };
            assert_eq!(snapshot.groups().len(), 1);
            let group = &snapshot.groups()[0];
            assert_eq!(group.kind(), TextGroupKind::DatabaseEntry);
            assert_eq!(group.fields()[0].field_name(), "name");
            assert_eq!(group.fields()[0].original_text(), "药水");
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
            [TrustedLuaExtractIntent::Replace(snapshot)] if snapshot.groups().is_empty()
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
            [TrustedLuaExtractIntent::Deactivate]
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
            [TrustedLuaExtractIntent::Deactivate]
        );

        let forged_observations = Arc::new(Mutex::new(TestObservations::default()));
        let forged = run_extract_program(
            &runtime,
            Arc::clone(&forged_observations),
            r#"
local items = ctx.mz.open(ctx.mz.data("Items.json"))
ctx.extract.replace_standard({{
  kind = "database_entry",
  location = items:location({1}),
  fields = {{name = "name", text = {original = "伪造"}}},
}})
"#,
        )
        .await
        .expect_err("普通 Lua table 不得伪造 MzTextRef");
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
assert(ctx.project.source_root == [[C:\projects\demo\source]])
assert(ctx.project.database_path == [[C:\projects\demo\project.db]])
assert(ctx.project.source_language == "ja")
assert(ctx.project.target_language == "zh-Hans")
assert(ctx.project.output_root == nil)
assert(ctx.output == nil and ctx.write_back == nil and ctx.extract == nil)
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
  local current = ctx.translation.prepare(kind, "kind:" .. kind)
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
local non_source = ctx.translation.prepare("map", "__non_source__")
assert(non_source.status == "non_source_language")
assert(non_source.model_text == "")
assert(#non_source.terms == 0)
local fully_protected = ctx.translation.prepare("map", "__fully_protected__")
assert(fully_protected.status == "fully_protected")
assert(fully_protected.model_text == "")
assert(#fully_protected.terms == 0)
local prepare_ok, prepare_error = pcall(
  ctx.translation.prepare,
  "map",
  "__prepare_error__"
)
assert(not prepare_ok)
assert(prepare_error.domain == "translation")
assert(prepare_error.kind == "prepare_failed")
local accepted = prepared:accept("勇者")
assert(accepted.accepted == true)
assert(accepted.translation == [=[勇者\C[2]]=])
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
                        "kind:database_entry".to_owned()
                    ),
                    (TextGroupKind::System, "kind:system".to_owned()),
                    (TextGroupKind::Map, "kind:map".to_owned()),
                    (TextGroupKind::EventDialogue, "kind:dialogue".to_owned()),
                    (TextGroupKind::EventChoices, "kind:choices".to_owned()),
                    (
                        TextGroupKind::EventScrollingText,
                        "kind:scrolling_text".to_owned()
                    ),
                    (TextGroupKind::EventCommand, "kind:event_command".to_owned()),
                    (
                        TextGroupKind::PluginParameter,
                        "kind:plugin_parameter".to_owned()
                    ),
                    (TextGroupKind::Map, "__non_source__".to_owned()),
                    (TextGroupKind::Map, "__fully_protected__".to_owned()),
                    (TextGroupKind::Map, "__prepare_error__".to_owned()),
                ]
            );
            let messages = &observation_guard.messages;
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].role(), ChatMessageRole::User);
            assert_eq!(messages[0].content(), "hello");
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
    async fn json_context_enforces_every_host_value_budget_dimension() {
        let runtime = TrustedLua54Runtime::new(
            test_configuration_with_budget(HostValueBudget::new(
                NonZeroUsize::new(32).unwrap(),
                NonZeroUsize::new(3).unwrap(),
                NonZeroUsize::new(2).unwrap(),
            )),
            Handle::current(),
        );
        let script = r#"
local json = ctx.json
assert(json.encode(json.decode("[1,2]")) == "[1,2]")
local failures = {
  function() return json.decode(string.rep(" ", 32) .. "0") end,
  function() return json.decode("[1,2,3]") end,
  function() return json.decode("[[1]]") end,
  function() return json.encode(string.rep("x", 40)) end,
  function() return json.encode(json.array({1, 2, 3})) end,
  function() return json.encode(json.array({json.array({1})})) end,
}
for _, failure in ipairs(failures) do
  local ok, error = pcall(failure)
  assert(not ok and error.domain == "json" and error.kind == "invalid_value")
end
"#;
        let report = runtime
            .start(
                OwnedLuaProgram::new(
                    PathBuf::from("C:/scripts/json-budget.lua"),
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
    async fn source_and_mz_facades_reuse_lossless_json_and_structured_locations() {
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

local item_source = ctx.mz.data("Items.json")
assert(type(item_source) == "userdata")
assert(tostring(item_source) == "data/Items.json")
local items = ctx.mz.open(item_source)
assert(type(items) == "userdata")
local value = items:value({1, "nested", ctx.mz.DECODE_JSON, 0, "Title"})
assert(value == "任务")
local location = items:location({1, "name"})
assert(type(location) == "userdata")
assert(tostring(location) == "data/Items.json[1].name")
local text = items:text({1, "name"})
assert(type(text) == "userdata")
assert(text.original == "药水")
assert(type(text.location) == "userdata")
assert(tostring(text.location) == "data/Items.json[1].name")
local note = items:note_tag({1}, "Help", 0)
assert(note.original == "恢复 HP")
assert(tostring(note.location) == "data/Items.json[1].note#Help[0]")

local map = ctx.mz.open(ctx.mz.map(1))
local comment = map:comment_tag({"list", 0}, "Quest", 0)
assert(comment.original == "第一\n行")
assert(tostring(comment.location) == "data/Map001.json.list[0]#comment:Quest[0]")

local plugin = ctx.mz.open(ctx.mz.plugin_parameter(0, "Quest", "Entries"))
local plugin_text = plugin:text({ctx.mz.DECODE_JSON, 0, "Title"})
assert(plugin_text.original == "插件任务")
assert(tostring(plugin_text.location) == "plugins.js[Quest].Entries<json>[0].Title")

local ok, error = pcall(ctx.mz.data, "Custom.json")
assert(not ok and error.domain == "mz" and error.kind == "invalid_source")
ok, error = pcall(ctx.mz.map, 0)
assert(not ok and error.domain == "mz" and error.kind == "invalid_source")
ok, error = pcall(ctx.mz.plugin_parameter, -1, "Quest", "Entries")
assert(not ok and error.domain == "mz" and error.kind == "invalid_plugin_parameter_source")
ok, error = pcall(ctx.mz.open, {})
assert(not ok and error.domain == "mz" and error.kind == "invalid_argument")
ok, error = pcall(items.value, items, {1, -1})
assert(not ok and error.domain == "mz" and error.kind == "invalid_argument")
ok, error = pcall(items.text, items, {1, "missing"})
assert(not ok and error.domain == "mz" and error.kind == "invalid_location")
"#;
        let report = runtime
            .start(
                OwnedLuaProgram::new(
                    PathBuf::from("C:/scripts/source-mz.lua"),
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

local prepared = ctx.translation.prepare("map", "text")
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
        let opened = crate::att_mz::project::OpenedProject::new(
            "demo".parse::<ProjectName>().unwrap(),
            PathBuf::from("C:/projects/demo"),
            PathBuf::from("C:/projects/demo/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            crate::att_mz::project::test_layout_profile(),
        );
        let calls = Arc::new(TestCalls {
            panic_on_project: false,
            project: LuaProjectContext::for_write_back_candidate(
                &opened,
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
                    b"assert(ctx.phase == 'extract'); assert(ctx.llm == nil); assert(ctx.translation == nil); assert(ctx.output == nil); assert(ctx.write_back == nil); assert(ctx.project.output_root == nil); assert(type(ctx.source) == 'table'); assert(type(ctx.mz) == 'table')".to_vec(),
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

        let opened = crate::att_mz::project::OpenedProject::new(
            "demo".parse::<ProjectName>().unwrap(),
            PathBuf::from("C:/projects/demo"),
            PathBuf::from("C:/projects/demo/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            crate::att_mz::project::test_layout_profile(),
        );
        let write_back_observations = Arc::new(Mutex::new(TestObservations::default()));
        let write_back_calls = Arc::new(TestCalls {
            panic_on_project: false,
            project: LuaProjectContext::for_write_back_candidate(
                &opened,
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
assert(ctx.project.output_root == 'C:/projects/demo/write_back')
assert(type(ctx.source) == 'table' and type(ctx.mz) == 'table')
assert(type(ctx.output) == 'table' and type(ctx.write_back) == 'table')

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
    async fn require_loads_a_lua_module_beside_the_main_script() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("local_helper.lua"),
            "return { value = 'loaded' }",
        )
        .unwrap();
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());
        let report = runtime
            .start(
                OwnedLuaProgram::new(
                    directory.path().join("main.lua"),
                    b"assert(require('local_helper').value == 'loaded')".to_vec(),
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

    #[test]
    fn native_module_symbols_follow_lua54_name_rules() {
        assert_eq!(native_module_symbols("plain"), ["luaopen_plain"]);
        assert_eq!(native_module_symbols("root.child"), ["luaopen_root_child"]);
        assert_eq!(
            native_module_symbols("versioned-v1.child"),
            ["luaopen_versioned", "luaopen_v1_child"]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn require_loads_native_modules_from_a_unicode_directory() {
        let directory = tempfile::tempdir().unwrap();
        let module_directory = directory.path().join("本地模块");
        std::fs::create_dir(&module_directory).unwrap();
        let library = compile_test_native_module(&module_directory);
        for name in ["root.dll", "versioned-v1.dll", "wrong_symbol.dll"] {
            std::fs::copy(&library, module_directory.join(name)).unwrap();
        }

        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current());
        let script = r#"
assert(#package.searchers == 4)
assert(require("unicode_native") == true)
assert(require("root.child") == true)
assert(require("versioned-v1") == true)
local ok, error = pcall(require, "wrong_symbol")
assert(not ok)
assert(string.find(tostring(error), "luaopen_wrong_symbol", 1, true))
"#;
        let report = runtime
            .start(
                OwnedLuaProgram::new(
                    module_directory.join("main.lua"),
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

    fn compile_test_native_module(directory: &Path) -> PathBuf {
        let source = directory.join("fixture.rs");
        std::fs::write(
            &source,
            r#"
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn luaopen_unicode_native(_: *mut core::ffi::c_void) -> i32 { 0 }

#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn luaopen_root_child(_: *mut core::ffi::c_void) -> i32 { 0 }

#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn luaopen_versioned(_: *mut core::ffi::c_void) -> i32 { 0 }
"#,
        )
        .unwrap();
        let library = directory.join("unicode_native.dll");
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let output = Command::new(rustc)
            .arg("--edition=2024")
            .arg("--crate-type=cdylib")
            .arg(&source)
            .arg("-o")
            .arg(&library)
            .output()
            .expect("测试环境必须能启动 rustc");
        assert!(
            output.status.success(),
            "无法编译 Lua C 测试模块：{}",
            String::from_utf8_lossy(&output.stderr)
        );
        library
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn configured_vm_memory_limit_rejects_excessive_allocation() {
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
        assert!(matches!(
            report.into_parts().0,
            Err(TrustedLuaRuntimeExecutionError::Execute(_))
        ));
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
}
