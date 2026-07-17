//! 使用专用 OS worker 运行完整 Lua 5.4 的生产根适配器。

use std::cell::RefCell;
use std::collections::HashMap;
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
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, oneshot};
use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

use crate::att_mz::lua::runtime::{
    OwnedLuaProgram, TrustedLuaBindingFinalizationError, TrustedLuaExecutionHandle,
    TrustedLuaHostCallError, TrustedLuaHostCalls, TrustedLuaRuntimeBindings,
    TrustedLuaRuntimeExecutionError, TrustedLuaRuntimeExecutionReport, TrustedLuaRuntimeExecutor,
    TrustedLuaRuntimeReservation, TrustedLuaRuntimeTermination,
};
use crate::att_mz::lua::{LuaPhase, LuaProjectContext};
use crate::llm::{ChatMessage, ChatMessageRole, LlmResponse, LlmUsage};
use crate::storage::sqlite::{SqliteCommand, SqliteQuery, SqliteRow, SqliteValue};

/// 已由配置边界建立的 Lua worker 资源上限。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TrustedLua54RuntimeConfiguration {
    worker_threads: NonZeroUsize,
    queue_capacity: NonZeroUsize,
    worker_stack_bytes: NonZeroUsize,
    memory_limit_bytes_per_vm: NonZeroUsize,
    cancel_check_instruction_interval: NonZeroU32,
    max_error_bytes: NonZeroUsize,
}

impl TrustedLua54RuntimeConfiguration {
    pub(crate) const fn new(
        worker_threads: NonZeroUsize,
        queue_capacity: NonZeroUsize,
        worker_stack_bytes: NonZeroUsize,
        memory_limit_bytes_per_vm: NonZeroUsize,
        cancel_check_instruction_interval: NonZeroU32,
        max_error_bytes: NonZeroUsize,
    ) -> Self {
        Self {
            worker_threads,
            queue_capacity,
            worker_stack_bytes,
            memory_limit_bytes_per_vm,
            cancel_check_instruction_interval,
            max_error_bytes,
        }
    }
}

/// Lua 生产根的构造、调度或 VM 失败。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TrustedLua54RuntimeError {
    CapacityOverflow,
    WorkerSpawn(String),
    ShuttingDown,
    WorkerChannelClosed,
    Context(String),
    Vm(String),
}

impl fmt::Display for TrustedLua54RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapacityOverflow => formatter.write_str("Lua worker 与队列容量溢出"),
            Self::WorkerSpawn(message) => write!(formatter, "无法创建 Lua worker：{message}"),
            Self::ShuttingDown => formatter.write_str("Lua Runtime 正在关闭"),
            Self::WorkerChannelClosed => formatter.write_str("Lua worker 通道已关闭"),
            Self::Context(message) => write!(formatter, "Lua 上下文无效：{message}"),
            Self::Vm(message) => formatter.write_str(message),
        }
    }
}

impl Error for TrustedLua54RuntimeError {}

type WorkerTask = Box<dyn FnOnce() + Send + 'static>;

struct RuntimeInner {
    sender: Mutex<Option<mpsc::Sender<WorkerTask>>>,
    capacity: Arc<Semaphore>,
    accepting: AtomicBool,
    shutdown_requested: Arc<AtomicBool>,
    runtime_handles: AtomicUsize,
    active_jobs: AtomicUsize,
    jobs_finished: Notify,
    tokio: Handle,
    memory_limit_bytes_per_vm: usize,
    cancel_check_instruction_interval: u32,
    max_error_bytes: usize,
}

/// 进程内完整 Lua 5.4 Runtime。
///
/// VM 与全部 Lua 标准库只在专用 OS worker 中存在。SQLite 与 LLM 调用通过
/// 同步响应桥交回构造本根的 Tokio Runtime 驱动。
pub(crate) struct TrustedLua54Runtime {
    inner: Arc<RuntimeInner>,
    workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl TrustedLua54Runtime {
    pub(crate) fn new(
        configuration: TrustedLua54RuntimeConfiguration,
        tokio: Handle,
    ) -> Result<Self, TrustedLua54RuntimeError> {
        let total_capacity = configuration
            .worker_threads
            .get()
            .checked_add(configuration.queue_capacity.get())
            .ok_or(TrustedLua54RuntimeError::CapacityOverflow)?;
        let (sender, receiver) = mpsc::channel::<WorkerTask>();
        let receiver = Arc::new(Mutex::new(receiver));
        let shutdown_requested = Arc::new(AtomicBool::new(false));
        let mut workers = Vec::with_capacity(configuration.worker_threads.get());

        for index in 0..configuration.worker_threads.get() {
            let receiver = Arc::clone(&receiver);
            let name = format!("att-lua-{index}");
            let worker = thread::Builder::new()
                .name(name)
                .stack_size(configuration.worker_stack_bytes.get())
                .spawn(move || worker_loop(&receiver));
            match worker {
                Ok(worker) => workers.push(worker),
                Err(error) => {
                    // 新 worker 失败后立即关闭唯一 sender，使已启动 worker 退出并完整 join。
                    drop(sender);
                    for worker in workers {
                        let _ = worker.join();
                    }
                    return Err(TrustedLua54RuntimeError::WorkerSpawn(error.to_string()));
                }
            }
        }

        Ok(Self {
            inner: Arc::new(RuntimeInner {
                sender: Mutex::new(Some(sender)),
                capacity: Arc::new(Semaphore::new(total_capacity)),
                accepting: AtomicBool::new(true),
                shutdown_requested,
                runtime_handles: AtomicUsize::new(1),
                active_jobs: AtomicUsize::new(0),
                jobs_finished: Notify::new(),
                tokio,
                memory_limit_bytes_per_vm: configuration.memory_limit_bytes_per_vm.get(),
                cancel_check_instruction_interval: configuration
                    .cancel_check_instruction_interval
                    .get(),
                max_error_bytes: configuration.max_error_bytes.get(),
            }),
            workers: Arc::new(Mutex::new(workers)),
        })
    }

    /// 停止新预留，取消已排队或正在执行的脚本，并等待 worker 退出。
    ///
    /// 可信脚本进入 native C 模块、`os.execute` 或替换调试 hook 后可以长时间不
    /// 交还控制；本方法不伪造超时成功。
    pub(crate) async fn shutdown(&self) -> Result<(), TrustedLua54RuntimeError> {
        self.inner.request_shutdown();

        let workers = {
            let mut guard = self.workers.lock().expect("Lua worker 锁不应中毒");
            std::mem::take(&mut *guard)
        };
        let joined = self
            .inner
            .tokio
            .spawn_blocking(move || {
                let mut panicked = false;
                for worker in workers {
                    panicked |= worker.join().is_err();
                }
                panicked
            })
            .await
            .map_err(|error| TrustedLua54RuntimeError::Vm(error.to_string()))?;
        loop {
            let finished = self.inner.jobs_finished.notified();
            if self.inner.active_jobs.load(Ordering::Acquire) == 0 {
                break;
            }
            finished.await;
        }
        if joined {
            Err(TrustedLua54RuntimeError::Vm(
                "Lua worker 在关闭期间 panic".to_owned(),
            ))
        } else {
            Ok(())
        }
    }
}

impl RuntimeInner {
    fn request_shutdown(&self) {
        self.accepting.store(false, Ordering::Release);
        self.shutdown_requested.store(true, Ordering::Release);
        self.capacity.close();
        match self.sender.lock() {
            Ok(mut sender) => {
                sender.take();
            }
            Err(poisoned) => {
                poisoned.into_inner().take();
            }
        }
    }
}

impl Clone for TrustedLua54Runtime {
    fn clone(&self) -> Self {
        self.inner.runtime_handles.fetch_add(1, Ordering::AcqRel);
        Self {
            inner: Arc::clone(&self.inner),
            workers: Arc::clone(&self.workers),
        }
    }
}

impl Drop for TrustedLua54Runtime {
    fn drop(&mut self) {
        if self.inner.runtime_handles.fetch_sub(1, Ordering::AcqRel) == 1 {
            // 显式 shutdown 才能取得 join 结果；最后一个句柄的兜底只负责停止准入、
            // 请求取消并关闭队列，避免遗留永不退出的 worker。
            self.inner.request_shutdown();
        }
    }
}

impl TrustedLuaRuntimeExecutor for TrustedLua54Runtime {
    type Error = TrustedLua54RuntimeError;
    type Reservation = TrustedLua54Reservation;

    async fn reserve(&self) -> Result<Self::Reservation, Self::Error> {
        if !self.inner.accepting.load(Ordering::Acquire) {
            return Err(TrustedLua54RuntimeError::ShuttingDown);
        }
        let permit = Arc::clone(&self.inner.capacity)
            .acquire_owned()
            .await
            .map_err(|_| TrustedLua54RuntimeError::ShuttingDown)?;
        // 先登记已经授予的 reservation，再复核准入开关。这样 shutdown 不会在
        // reserve 与 start 之间误判为已经没有受理中的工作。
        self.inner.active_jobs.fetch_add(1, Ordering::AcqRel);
        if !self.inner.accepting.load(Ordering::Acquire) {
            self.inner.active_jobs.fetch_sub(1, Ordering::AcqRel);
            self.inner.jobs_finished.notify_waiters();
            return Err(TrustedLua54RuntimeError::ShuttingDown);
        }
        Ok(TrustedLua54Reservation {
            inner: Arc::clone(&self.inner),
            permit: Some(permit),
        })
    }
}

/// 一次不可克隆的 Lua 容量预留。
pub(crate) struct TrustedLua54Reservation {
    inner: Arc<RuntimeInner>,
    permit: Option<OwnedSemaphorePermit>,
}

impl Drop for TrustedLua54Reservation {
    fn drop(&mut self) {
        if self.permit.is_some() {
            self.inner.active_jobs.fetch_sub(1, Ordering::AcqRel);
            self.inner.jobs_finished.notify_waiters();
        }
    }
}

impl TrustedLuaRuntimeReservation for TrustedLua54Reservation {
    type Error = TrustedLua54RuntimeError;

    fn start(
        mut self,
        program: OwnedLuaProgram,
        bindings: TrustedLuaRuntimeBindings,
    ) -> TrustedLuaExecutionHandle<Self::Error> {
        let (calls, finalizer) = bindings.into_parts();
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation = RuntimeCancellation {
            local: Arc::clone(&cancelled),
            shutdown: Arc::clone(&self.inner.shutdown_requested),
        };
        let (worker_sender, worker_receiver) = oneshot::channel::<WorkerOutcome>();
        let (report_sender, report_receiver) = oneshot::channel();
        let supervisor_cancel = cancellation.clone();
        let permit = self
            .permit
            .take()
            .expect("Lua reservation permit 只能移交一次");
        let supervisor_inner = Arc::clone(&self.inner);

        self.inner.tokio.spawn(async move {
            let runtime = match worker_receiver.await {
                Ok(result) => result.into_runtime_result(),
                Err(_) => Err(TrustedLuaRuntimeExecutionError::Unavailable(
                    TrustedLua54RuntimeError::WorkerChannelClosed,
                )),
            };
            let termination = if supervisor_cancel.is_cancelled()
                || matches!(runtime, Err(TrustedLuaRuntimeExecutionError::Cancelled))
            {
                TrustedLuaRuntimeTermination::Cancelled
            } else if runtime.is_ok() {
                TrustedLuaRuntimeTermination::Completed
            } else {
                TrustedLuaRuntimeTermination::Failed
            };
            let finalization =
                match catch_unwind(AssertUnwindSafe(|| finalizer.finalize(termination))) {
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
            let _ =
                report_sender.send(TrustedLuaRuntimeExecutionReport::new(runtime, finalization));
            drop(permit);
            supervisor_inner.active_jobs.fetch_sub(1, Ordering::AcqRel);
            supervisor_inner.jobs_finished.notify_waiters();
        });

        let tokio = self.inner.tokio.clone();
        let memory_limit_bytes_per_vm = self.inner.memory_limit_bytes_per_vm;
        let cancel_check_instruction_interval = self.inner.cancel_check_instruction_interval;
        let max_error_bytes = self.inner.max_error_bytes;
        let task: WorkerTask = Box::new(move || {
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                execute_program(
                    &program,
                    calls,
                    &tokio,
                    &cancellation,
                    memory_limit_bytes_per_vm,
                    cancel_check_instruction_interval,
                    max_error_bytes,
                )
            }))
            .unwrap_or(WorkerOutcome::Panicked);
            let _ = worker_sender.send(outcome);
        });

        let sent = self
            .inner
            .sender
            .lock()
            .expect("Lua sender 锁不应中毒")
            .as_ref()
            .is_some_and(|sender| sender.send(task).is_ok());
        if !sent {
            // task 被丢弃后 worker_sender 同步关闭，supervisor 仍会执行唯一终结器。
        }

        TrustedLuaExecutionHandle::new(report_receiver, cancelled)
    }
}

fn worker_loop(receiver: &Mutex<mpsc::Receiver<WorkerTask>>) {
    loop {
        let task = {
            let guard = receiver.lock().expect("Lua receiver 锁不应中毒");
            guard.recv()
        };
        let Ok(task) = task else {
            return;
        };
        task();
    }
}

#[derive(Clone)]
struct RuntimeCancellation {
    local: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
}

impl RuntimeCancellation {
    fn is_cancelled(&self) -> bool {
        self.local.load(Ordering::Acquire) || self.shutdown.load(Ordering::Acquire)
    }
}

enum WorkerOutcome {
    Completed,
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
    calls: Arc<dyn TrustedLuaHostCalls>,
    tokio: &Handle,
    cancellation: &RuntimeCancellation,
    memory_limit_bytes_per_vm: usize,
    cancel_check_instruction_interval: u32,
    max_error_bytes: usize,
) -> WorkerOutcome {
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

    let context = match build_context(
        &lua,
        Arc::clone(&calls),
        tokio.clone(),
        cancellation.clone(),
    ) {
        Ok(context) => context,
        Err(error) => {
            return WorkerOutcome::Context(vm_error("无法构造 Lua ctx", error, max_error_bytes));
        }
    };
    if let Err(error) = lua.globals().set("ctx", context) {
        return WorkerOutcome::Context(vm_error("无法注入 Lua ctx", error, max_error_bytes));
    }

    let function = match lua
        .load(program.source())
        .set_name(&script_path)
        .into_function()
    {
        Ok(function) => function,
        Err(error) => {
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
    calls: Arc<dyn TrustedLuaHostCalls>,
    tokio: Handle,
    cancellation: RuntimeCancellation,
) -> mlua::Result<Table> {
    let context = lua.create_table()?;
    context.set("phase", phase_name(calls.phase()))?;
    context.set("project", build_project_table(lua, calls.project())?)?;
    context.set(
        "db",
        build_database_table(lua, Arc::clone(&calls), tokio.clone(), cancellation.clone())?,
    )?;
    if calls.phase() == LuaPhase::Translate {
        context.set("llm", build_llm_function(lua, calls, tokio, cancellation)?)?;
    }
    Ok(context)
}

fn phase_name(phase: LuaPhase) -> &'static str {
    match phase {
        LuaPhase::Extract => "extract",
        LuaPhase::Translate => "translate",
        LuaPhase::WriteBack => "write_back",
    }
}

fn build_project_table(lua: &Lua, project: &LuaProjectContext) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("name", project.name().as_str())?;
    table.set("source_root", strict_path(project.source_root())?)?;
    table.set("database_path", strict_path(project.database_path())?)?;
    table.set("source_language", project.source_language())?;
    table.set("target_language", project.target_language())?;
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
    calls: Arc<dyn TrustedLuaHostCalls>,
    tokio: Handle,
    cancellation: RuntimeCancellation,
) -> mlua::Result<Table> {
    let native = lua.create_table()?;
    let null = lua.create_userdata(LuaNull)?;

    let query_calls = Arc::clone(&calls);
    let query_tokio = tokio.clone();
    let query_cancellation = cancellation.clone();
    native.set(
        "query",
        lua.create_function(move |lua, (statement, parameters): (Value, Value)| {
            let result =
                parse_sql_call(statement, parameters).and_then(|(statement, parameters)| {
                    wait_for_host(
                        &query_tokio,
                        &query_cancellation,
                        query_calls.query(SqliteQuery::new(statement, parameters)),
                    )
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
            let result =
                parse_sql_call(statement, parameters).and_then(|(statement, parameters)| {
                    wait_for_host(
                        &execute_tokio,
                        &execute_cancellation,
                        execute_calls.execute(SqliteCommand::new(statement, parameters)),
                    )
                });
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

    let blob = lua.create_function(|lua, bytes: Value| {
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
    calls: Arc<dyn TrustedLuaHostCalls>,
    tokio: Handle,
    cancellation: RuntimeCancellation,
) -> mlua::Result<Function> {
    let native = lua.create_function(move |lua, messages: Value| {
        let result = parse_message_array(messages)
            .and_then(|messages| wait_for_host(&tokio, &cancellation, calls.request_llm(messages)));
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
    let parameters = parse_parameters(parameters).map_err(binding_error)?;
    Ok((statement, parameters))
}

fn parse_parameters(value: Value) -> mlua::Result<Vec<SqliteValue>> {
    match value {
        Value::Nil => Ok(Vec::new()),
        Value::Table(table) => dense_values(table)?
            .into_iter()
            .map(lua_to_sqlite_value)
            .collect(),
        other => Err(mlua::Error::runtime(format!(
            "SQLite parameters 必须是无洞数组或 nil，实际为 {}",
            other.type_name()
        ))),
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

fn lua_to_sqlite_value(value: Value) -> mlua::Result<SqliteValue> {
    match value {
        Value::Integer(value) => Ok(SqliteValue::Integer(value)),
        Value::Number(value) if value.is_finite() => Ok(SqliteValue::Real(value)),
        Value::Number(_) => Err(mlua::Error::runtime("SQLite REAL 参数不得为 NaN 或 Inf")),
        Value::String(value) => Ok(SqliteValue::Text(lua_string_to_text(&value, "TEXT")?)),
        Value::UserData(value) if value.is::<LuaNull>() => Ok(SqliteValue::Null),
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
        SqliteValue::Null => Ok(Value::UserData(lua.create_userdata(LuaNull)?)),
        SqliteValue::Integer(value) => Ok(Value::Integer(value)),
        SqliteValue::Real(value) if value.is_finite() => Ok(Value::Number(value)),
        SqliteValue::Real(_) => Err(mlua::Error::runtime("SQLite REAL 结果为 NaN 或 Inf")),
        SqliteValue::Text(value) => Ok(Value::String(lua.create_string(value)?)),
        SqliteValue::Blob(value) => Ok(Value::UserData(lua.create_userdata(LuaBlob(value))?)),
    }
}

fn parse_messages(table: Table) -> mlua::Result<Vec<ChatMessage>> {
    dense_values(table)?
        .into_iter()
        .map(|value| {
            let Value::Table(message) = value else {
                return Err(mlua::Error::runtime("LLM messages 的每一项必须是 table"));
            };
            ensure_exact_string_keys(&message, &["role", "content"])?;
            let role: mlua::LuaString = message.get("role")?;
            let content: mlua::LuaString = message.get("content")?;
            let role = match role.to_str()?.as_ref() {
                "system" => ChatMessageRole::System,
                "user" => ChatMessageRole::User,
                "assistant" => ChatMessageRole::Assistant,
                _ => return Err(mlua::Error::runtime("LLM message.role 无效")),
            };
            Ok(ChatMessage::new(
                role,
                lua_string_to_text(&content, "message.content")?,
            ))
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
    parse_messages(messages).map_err(binding_error)
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
    table.set("response_id", response.provider_response_id())?;
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

#[derive(Clone, Copy, Debug)]
struct LuaNull;

impl UserData for LuaNull {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::Eq, |_lua, _this, other: AnyUserData| {
            Ok(other.is::<LuaNull>())
        });
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
    use std::task::Poll;
    use std::time::Duration;

    use crate::att_mz::ProjectName;
    use crate::att_mz::lua::runtime::{
        TrustedLuaBindingFinalization, TrustedLuaBindingFinalizer, TrustedLuaRuntimeBindings,
    };
    use crate::project_database::StoredProjectRecord;

    #[derive(Default)]
    struct TestObservations {
        executed_parameters: Vec<SqliteValue>,
        messages: Vec<ChatMessage>,
    }

    struct TestCalls {
        phase: LuaPhase,
        panic_on_phase: bool,
        project: LuaProjectContext,
        observations: Arc<Mutex<TestObservations>>,
        begin_error: Option<TrustedLuaHostCallError>,
        begin_started: Option<Arc<Notify>>,
        begin_gate: Option<Arc<Notify>>,
    }

    impl TrustedLuaHostCalls for TestCalls {
        fn phase(&self) -> LuaPhase {
            assert!(!self.panic_on_phase, "测试请求 Host phase panic");
            self.phase
        }

        fn project(&self) -> &LuaProjectContext {
            &self.project
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
                    "response-1",
                    Some(LlmUsage::new(3, 5, 8)),
                ))
            })
        }
    }

    struct TestFinalizer {
        terminations: Arc<Mutex<Vec<TrustedLuaRuntimeTermination>>>,
        completion: Option<oneshot::Sender<TrustedLuaRuntimeTermination>>,
    }

    impl TrustedLuaBindingFinalizer for TestFinalizer {
        fn finalize(
            self: Box<Self>,
            termination: TrustedLuaRuntimeTermination,
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
                terminations,
                completion,
            } = *self;
            Box::pin(async move {
                terminations
                    .lock()
                    .expect("终结记录锁不应中毒")
                    .push(termination);
                if let Some(completion) = completion {
                    let _ = completion.send(termination);
                }
                Ok(TrustedLuaBindingFinalization::new(false))
            })
        }
    }

    struct PanickingFinalizer;

    impl TrustedLuaBindingFinalizer for PanickingFinalizer {
        fn finalize(
            self: Box<Self>,
            _termination: TrustedLuaRuntimeTermination,
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
            NonZeroUsize::new(1).unwrap(),
            NonZeroUsize::new(1).unwrap(),
            NonZeroUsize::new(2 * 1024 * 1024).unwrap(),
            NonZeroUsize::new(16 * 1024 * 1024).unwrap(),
            NonZeroU32::new(100).unwrap(),
            NonZeroUsize::new(4096).unwrap(),
        )
    }

    fn test_project() -> LuaProjectContext {
        LuaProjectContext::from_stored_record(&StoredProjectRecord::new(
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
        terminations: Arc<Mutex<Vec<TrustedLuaRuntimeTermination>>>,
        completion: Option<oneshot::Sender<TrustedLuaRuntimeTermination>>,
    ) -> TrustedLuaRuntimeBindings {
        TrustedLuaRuntimeBindings::new(
            Arc::new(TestCalls {
                phase: LuaPhase::Translate,
                panic_on_phase: false,
                project: test_project(),
                observations,
                begin_error,
                begin_started: None,
                begin_gate: None,
            }),
            Box::new(TestFinalizer {
                terminations,
                completion,
            }),
        )
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
            .raw_set(3, lua.create_userdata(LuaNull).unwrap())
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
    fn error_truncation_preserves_utf8() {
        assert_eq!(truncate_utf8("中文abc".to_owned(), 4), "中");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_vm_exposes_exact_ctx_and_preserves_sqlite_and_llm_values() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current()).unwrap();
        let observations = Arc::new(Mutex::new(TestObservations::default()));
        let terminations = Arc::new(Mutex::new(Vec::new()));
        let reservation = runtime.reserve().await.unwrap();
        let script = r#"
assert(ctx.phase == "translate")
assert(ctx.project.name == "demo")
assert(ctx.project.source_root == [[C:\projects\demo\source]])
assert(ctx.project.database_path == [[C:\projects\demo\project.db]])
assert(ctx.project.source_language == "ja")
assert(ctx.project.target_language == "zh-Hans")
assert(ctx.project.output_root == nil)
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
        let report = reservation
            .start(
                OwnedLuaProgram::new(
                    PathBuf::from("C:/scripts/main.lua"),
                    script.as_bytes().to_vec(),
                ),
                test_bindings(
                    None,
                    Arc::clone(&observations),
                    Arc::clone(&terminations),
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
            let messages = &observation_guard.messages;
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].role(), ChatMessageRole::User);
            assert_eq!(messages[0].content(), "hello");
        }
        assert_eq!(
            *terminations.lock().unwrap(),
            vec![TrustedLuaRuntimeTermination::Completed]
        );
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pcall_receives_typed_host_error_and_unhandled_error_is_binding_failure() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current()).unwrap();
        let host_error =
            TrustedLuaHostCallError::new("sqlite", "busy", "database busy", Some(25), None);
        let caught = runtime
            .reserve()
            .await
            .unwrap()
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
            .reserve()
            .await
            .unwrap()
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

        let invalid_binding = runtime
            .reserve()
            .await
            .unwrap()
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
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current()).unwrap();
        let (completion, finalized) = oneshot::channel();
        let handle = runtime.reserve().await.unwrap().start(
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
        let termination = tokio::time::timeout(Duration::from_secs(5), finalized)
            .await
            .expect("取消后应完成唯一终结")
            .expect("终结器应发送终态");
        assert_eq!(termination, TrustedLuaRuntimeTermination::Cancelled);
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_the_last_runtime_handle_requests_cancellation_and_finalization() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current()).unwrap();
        let (completion, finalized) = oneshot::channel();
        let execution = runtime.reserve().await.unwrap().start(
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
            TrustedLuaRuntimeTermination::Cancelled
        );
        let (runtime, finalization) = execution.await.into_parts();
        assert!(matches!(
            runtime,
            Err(TrustedLuaRuntimeExecutionError::Cancelled)
        ));
        finalization.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancellation_interrupts_a_host_await_and_then_finalizes() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current()).unwrap();
        let started = Arc::new(Notify::new());
        let gate = Arc::new(Notify::new());
        let (completion, finalized) = oneshot::channel();
        let calls: Arc<dyn TrustedLuaHostCalls> = Arc::new(TestCalls {
            phase: LuaPhase::Translate,
            panic_on_phase: false,
            project: test_project(),
            observations: Arc::new(Mutex::new(TestObservations::default())),
            begin_error: None,
            begin_started: Some(Arc::clone(&started)),
            begin_gate: Some(gate),
        });
        let bindings = TrustedLuaRuntimeBindings::new(
            calls,
            Box::new(TestFinalizer {
                terminations: Arc::new(Mutex::new(Vec::new())),
                completion: Some(completion),
            }),
        );
        let handle = runtime.reserve().await.unwrap().start(
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
            TrustedLuaRuntimeTermination::Cancelled
        );
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_vm_only_exposes_llm_in_translate_and_output_in_write_back() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current()).unwrap();
        let terminations = Arc::new(Mutex::new(Vec::new()));
        let extract_calls: Arc<dyn TrustedLuaHostCalls> = Arc::new(TestCalls {
            phase: LuaPhase::Extract,
            panic_on_phase: false,
            project: test_project(),
            observations: Arc::new(Mutex::new(TestObservations::default())),
            begin_error: None,
            begin_started: None,
            begin_gate: None,
        });
        let extract = runtime
            .reserve()
            .await
            .unwrap()
            .start(
                OwnedLuaProgram::new(
                    PathBuf::from("C:/scripts/extract.lua"),
                    b"assert(ctx.phase == 'extract'); assert(ctx.llm == nil); assert(ctx.project.output_root == nil)".to_vec(),
                ),
                TrustedLuaRuntimeBindings::new(
                    extract_calls,
                    Box::new(TestFinalizer {
                        terminations: Arc::clone(&terminations),
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
        let write_back_calls: Arc<dyn TrustedLuaHostCalls> = Arc::new(TestCalls {
            phase: LuaPhase::WriteBack,
            panic_on_phase: false,
            project: LuaProjectContext::for_published_write_back(
                &opened,
                PathBuf::from("C:/projects/demo/write_back"),
            ),
            observations: Arc::new(Mutex::new(TestObservations::default())),
            begin_error: None,
            begin_started: None,
            begin_gate: None,
        });
        let write_back = runtime
            .reserve()
            .await
            .unwrap()
            .start(
                OwnedLuaProgram::new(
                    PathBuf::from("C:/scripts/write.lua"),
                    b"assert(ctx.phase == 'write_back'); assert(ctx.llm == nil); assert(ctx.project.output_root == 'C:/projects/demo/write_back')".to_vec(),
                ),
                TrustedLuaRuntimeBindings::new(
                    write_back_calls,
                    Box::new(TestFinalizer {
                        terminations: Arc::clone(&terminations),
                        completion: None,
                    }),
                ),
            )
            .await;
        write_back.into_parts().0.unwrap();
        assert_eq!(
            *terminations.lock().unwrap(),
            vec![
                TrustedLuaRuntimeTermination::Completed,
                TrustedLuaRuntimeTermination::Completed,
            ]
        );
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn worker_panic_is_isolated_and_still_runs_the_unique_finalizer() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current()).unwrap();
        let terminations = Arc::new(Mutex::new(Vec::new()));
        let calls: Arc<dyn TrustedLuaHostCalls> = Arc::new(TestCalls {
            phase: LuaPhase::Extract,
            panic_on_phase: true,
            project: test_project(),
            observations: Arc::new(Mutex::new(TestObservations::default())),
            begin_error: None,
            begin_started: None,
            begin_gate: None,
        });
        let report = runtime
            .reserve()
            .await
            .unwrap()
            .start(
                OwnedLuaProgram::new(
                    PathBuf::from("C:/scripts/panic.lua"),
                    b"return true".to_vec(),
                ),
                TrustedLuaRuntimeBindings::new(
                    calls,
                    Box::new(TestFinalizer {
                        terminations: Arc::clone(&terminations),
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
        assert_eq!(
            *terminations.lock().unwrap(),
            [TrustedLuaRuntimeTermination::Failed]
        );
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn synchronous_finalizer_panic_becomes_a_cleanup_report() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current()).unwrap();
        let calls: Arc<dyn TrustedLuaHostCalls> = Arc::new(TestCalls {
            phase: LuaPhase::Extract,
            panic_on_phase: false,
            project: test_project(),
            observations: Arc::new(Mutex::new(TestObservations::default())),
            begin_error: None,
            begin_started: None,
            begin_gate: None,
        });
        let report = runtime
            .reserve()
            .await
            .unwrap()
            .start(
                OwnedLuaProgram::new(
                    PathBuf::from("C:/scripts/finalizer-panic.lua"),
                    b"return true".to_vec(),
                ),
                TrustedLuaRuntimeBindings::new(calls, Box::new(PanickingFinalizer)),
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
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current()).unwrap();
        let report = runtime
            .reserve()
            .await
            .unwrap()
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

        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current()).unwrap();
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
            .reserve()
            .await
            .unwrap()
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
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current()).unwrap();
        let report = runtime
            .reserve()
            .await
            .unwrap()
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

    #[tokio::test]
    async fn reservations_apply_worker_plus_queue_backpressure() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current()).unwrap();
        let first = runtime.reserve().await.unwrap();
        let second = runtime.reserve().await.unwrap();
        let mut third = Box::pin(runtime.reserve());
        assert!(matches!(futures_util::poll!(&mut third), Poll::Pending));
        drop(first);
        let third = third.await.unwrap();
        drop(second);
        drop(third);
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_waits_for_a_granted_reservation_to_be_released() {
        let runtime = TrustedLua54Runtime::new(test_configuration(), Handle::current()).unwrap();
        let reservation = runtime.reserve().await.unwrap();
        let shutdown_runtime = runtime.clone();
        let mut shutdown = tokio::spawn(async move { shutdown_runtime.shutdown().await });

        tokio::task::yield_now().await;
        assert!(matches!(futures_util::poll!(&mut shutdown), Poll::Pending));

        drop(reservation);
        shutdown.await.unwrap().unwrap();
    }
}
