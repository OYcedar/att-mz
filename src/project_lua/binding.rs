use std::cell::{Cell, RefCell};
use std::cmp::Ordering as CmpOrdering;
use std::collections::{BTreeMap, HashSet};
use std::ffi::{CString, c_char, c_void};
use std::num::NonZeroU32;
use std::ptr;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use mlua::thread::ThreadStatus;
use mlua::{
    AnyUserData, Function, HookTriggers, Lua, LuaOptions, MetaMethod, MultiValue, StdLib, Table,
    UserData, UserDataFields, UserDataMethods, Value, VmState,
};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, params_from_iter};

use super::{
    PROJECT_LUA_SOURCE_CHUNK_BYTES, ProjectLuaCallError, ProjectLuaCancellation,
    ProjectLuaEngineAdapter, ProjectLuaFailure, ProjectLuaPrintSink, ProjectLuaProgram,
    ProjectLuaRunRequest, ProjectLuaValue,
};

pub(super) struct PreparedProjectLua {
    pub(super) lua: Lua,
    pub(super) function: Function,
    pub(super) connection: Rc<RefCell<Connection>>,
    pub(super) metrics: Arc<BindingMetrics>,
    pub(super) transaction_guard: Rc<BindingTransactionGuard>,
}

#[derive(Debug, Default)]
pub(super) struct BindingMetrics {
    database_calls: AtomicU64,
    changed_rows: AtomicU64,
    translation_calls: AtomicU64,
    printed_lines: AtomicU64,
}

impl BindingMetrics {
    pub(super) fn database_calls(&self) -> u64 {
        self.database_calls.load(Ordering::Relaxed)
    }

    pub(super) fn changed_rows(&self) -> u64 {
        self.changed_rows.load(Ordering::Relaxed)
    }

    pub(super) fn translation_calls(&self) -> u64 {
        self.translation_calls.load(Ordering::Relaxed)
    }

    pub(super) fn printed_lines(&self) -> u64 {
        self.printed_lines.load(Ordering::Relaxed)
    }

    fn record_database_call(&self) {
        self.database_calls
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                Some(value.saturating_add(1))
            })
            .expect("计数更新闭包始终返回新值");
    }

    fn record_changed_rows(&self, rows: u64) {
        self.changed_rows
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                Some(value.saturating_add(rows))
            })
            .expect("计数更新闭包始终返回新值");
    }

    fn record_translation_call(&self) {
        self.translation_calls
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                Some(value.saturating_add(1))
            })
            .expect("计数更新闭包始终返回新值");
    }

    fn record_printed_line(&self) {
        self.printed_lines
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                Some(value.saturating_add(1))
            })
            .expect("计数更新闭包始终返回新值");
    }
}

/// 记录脚本是否让 ATT 建立的外层事务提前结束。
///
/// SQLite 的 `OR ROLLBACK` 和触发器 `RAISE(ROLLBACK)` 可以在一条普通 statement
/// 内结束外层事务。Lua 可以用 `pcall` 捕获 statement 错误，因此每个数据库入口都必须
/// 共用这一状态；一旦事务丢失，后续入口不得在 autocommit 模式下继续写入。
#[derive(Debug, Default)]
pub(super) struct BindingTransactionGuard {
    lost: Cell<bool>,
}

impl BindingTransactionGuard {
    pub(super) fn is_lost(&self) -> bool {
        self.lost.get()
    }

    fn call<T>(
        &self,
        connection: &Connection,
        operation: &'static str,
        call: impl FnOnce(&Connection) -> Result<T, LuaHostCallError>,
    ) -> Result<T, LuaHostCallError> {
        if self.lost.get() || connection.is_autocommit() {
            self.lost.set(true);
            return Err(transaction_lost_error(operation));
        }

        let result = call(connection);
        if connection.is_autocommit() {
            self.lost.set(true);
            Err(transaction_lost_error(operation))
        } else {
            result
        }
    }
}

#[derive(Debug)]
struct ProjectLuaCancellationTrap;

impl std::fmt::Display for ProjectLuaCancellationTrap {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ATT 中止了 Lua 脚本")
    }
}

impl std::error::Error for ProjectLuaCancellationTrap {}

pub(super) fn prepare_lua(
    connection: Connection,
    request: &ProjectLuaRunRequest,
    cancel_check_instruction_interval: NonZeroU32,
) -> Result<PreparedProjectLua, ProjectLuaFailure> {
    let lua = new_restricted_lua()?;

    let cancellation = request.cancellation.clone();
    install_cancellation_guards(&lua, cancellation.clone())
        .map_err(|error| ProjectLuaFailure::Context(error.to_string()))?;
    let hook_cancellation = cancellation.clone();
    lua.set_global_hook(
        HookTriggers::new().every_nth_instruction(cancel_check_instruction_interval.get()),
        move |lua, _debug| {
            if !hook_cancellation.is_cancelled() {
                return Ok(VmState::Continue);
            }

            let is_yieldable = lua.exec_raw_lua(|lua| {
                // SAFETY: 这里只读取 mlua 在 hook 期间设置的当前 coroutine 状态，
                // 不保留指针，也不修改 Lua 栈或 VM。
                unsafe { mlua::ffi::lua_isyieldable(lua.state()) != 0 }
            });
            if is_yieldable {
                Ok(VmState::Yield)
            } else {
                // Lua 5.4 不允许跨某些 C/metamethod 边界 yield。先用脚本无法伪造的
                // Rust 错误退出该边界；取消标记保持有效，回到可 yield 的 Lua 后，
                // 下一次 hook 会把宿主持有的执行 coroutine 挂起。
                Err(mlua::Error::external(ProjectLuaCancellationTrap))
            }
        },
    )
    .map_err(|error| ProjectLuaFailure::Context(error.to_string()))?;

    let function = compile_program(&lua, &request.program, &request.cancellation)?;

    let connection = Rc::new(RefCell::new(connection));
    let metrics = Arc::clone(&request.metrics);
    let transaction_guard = Rc::new(BindingTransactionGuard::default());
    let context = build_context(
        &lua,
        request,
        Rc::clone(&connection),
        Arc::clone(&metrics),
        Rc::clone(&transaction_guard),
    )
    .map_err(|error| ProjectLuaFailure::Context(error.to_string()))?;
    lua.globals()
        .set("ctx", context)
        .map_err(|error| ProjectLuaFailure::Context(error.to_string()))?;
    install_arguments(&lua, request)
        .map_err(|error| ProjectLuaFailure::Context(error.to_string()))?;
    install_print(
        &lua,
        Arc::clone(&request.print_sink),
        Arc::clone(&metrics),
        cancellation,
    )
    .map_err(|error| ProjectLuaFailure::Context(error.to_string()))?;

    Ok(PreparedProjectLua {
        lua,
        function,
        connection,
        metrics,
        transaction_guard,
    })
}

pub(super) fn validate_program(
    program: &ProjectLuaProgram,
    cancellation: &ProjectLuaCancellation,
) -> Result<(), ProjectLuaFailure> {
    let lua = new_restricted_lua()?;
    let _function = compile_program(&lua, program, cancellation)?;
    Ok(())
}

fn new_restricted_lua() -> Result<Lua, ProjectLuaFailure> {
    let libraries =
        StdLib::COROUTINE | StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8;
    let lua = Lua::new_with(libraries, LuaOptions::default())
        .map_err(|error| ProjectLuaFailure::Context(error.to_string()))?;
    remove_external_capabilities(&lua)?;
    Ok(lua)
}

fn install_cancellation_guards(
    lua: &Lua,
    cancellation: ProjectLuaCancellation,
) -> mlua::Result<()> {
    let checkpoint =
        lua.create_function(move |_lua, ()| ensure_lua_not_cancelled(&cancellation))?;
    let installer: Function = lua
        .load(
            r#"
return function(checkpoint)
  local pack = table.pack
  local unpack = table.unpack
  local raise = error
  local original_pcall = pcall
  local original_xpcall = xpcall
  local original_create = coroutine.create
  local original_resume = coroutine.resume
  local original_yield = coroutine.yield

  pcall = function(...)
    checkpoint()
    local results = pack(original_pcall(...))
    checkpoint()
    return unpack(results, 1, results.n)
  end

  xpcall = function(call, handler, ...)
    checkpoint()
    local function guarded_handler(value)
      checkpoint()
      return handler(value)
    end
    local results = pack(original_xpcall(call, guarded_handler, ...))
    checkpoint()
    return unpack(results, 1, results.n)
  end

  coroutine.create = function(...)
    checkpoint()
    local thread = original_create(...)
    checkpoint()
    return thread
  end

  coroutine.resume = function(...)
    checkpoint()
    local results = pack(original_resume(...))
    checkpoint()
    return unpack(results, 1, results.n)
  end

  coroutine.yield = function(...)
    checkpoint()
    local results = pack(original_yield(...))
    checkpoint()
    return unpack(results, 1, results.n)
  end

  coroutine.wrap = function(body)
    checkpoint()
    local thread = original_create(body)
    checkpoint()
    return function(...)
      checkpoint()
      local results = pack(original_resume(thread, ...))
      checkpoint()
      if not results[1] then raise(results[2], 0) end
      return unpack(results, 2, results.n)
    end
  end
end
"#,
        )
        .eval()?;
    installer.call(checkpoint)
}

fn ensure_lua_not_cancelled(cancellation: &ProjectLuaCancellation) -> mlua::Result<()> {
    if cancellation.is_cancelled() {
        Err(mlua::Error::external(ProjectLuaCancellationTrap))
    } else {
        Ok(())
    }
}

struct CancellableLuaSourceReader<'source> {
    source: &'source [u8],
    cancellation: ProjectLuaCancellation,
    position: usize,
    cancelled: bool,
}

unsafe extern "C-unwind" fn read_lua_source_chunk(
    _state: *mut mlua::ffi::lua_State,
    data: *mut c_void,
    size: *mut usize,
) -> *const c_char {
    // SAFETY: `compile_program` 在同步 `lua_load` 调用期间传入唯一的 reader 指针，
    // reader 及其借用的 program source 在整个回调期内都保持存活。
    let reader = unsafe { &mut *data.cast::<CancellableLuaSourceReader<'_>>() };
    if reader.cancellation.is_cancelled() {
        reader.cancelled = true;
        // SAFETY: Lua 按 lua_Reader 契约传入有效的 size 输出指针。
        unsafe { *size = 0 };
        return ptr::null();
    }
    if reader.position == reader.source.len() {
        // EOF 前仍执行了上面的取消检查，避免最后一块之后漏掉取消。
        unsafe { *size = 0 };
        return ptr::null();
    }

    let length = (reader.source.len() - reader.position).min(PROJECT_LUA_SOURCE_CHUNK_BYTES.get());
    // SAFETY: position 小于 source.len()，length 未超过剩余 slice；compile_program 在
    // 同步 lua_load 期间持续借用且不修改 source Vec，因此其缓冲区地址保持稳定。
    let chunk = unsafe { reader.source.as_ptr().add(reader.position) };
    reader.position += length;
    // SAFETY: Lua 按 lua_Reader 契约传入有效的 size 输出指针。
    unsafe { *size = length };
    chunk.cast()
}

fn compile_program(
    lua: &Lua,
    program: &ProjectLuaProgram,
    cancellation: &ProjectLuaCancellation,
) -> Result<Function, ProjectLuaFailure> {
    validate_lua_source_utf8(program.source(), cancellation)?;
    let name = CString::new(program.identity())
        .map_err(|error| ProjectLuaFailure::Compile(format!("invalid name: {error}")))?;
    let mut reader = CancellableLuaSourceReader {
        source: program.source(),
        cancellation: cancellation.clone(),
        position: 0,
        cancelled: false,
    };
    let reader_data = (&mut reader as *mut CancellableLuaSourceReader).cast::<c_void>();
    // SAFETY: 闭包成功时只在 Lua 栈留下一个值；lua_load 不持有 reader 数据，并且只会
    // 在本次同步调用返回前调用 read_lua_source_chunk。
    let result: mlua::Result<Function> = unsafe {
        lua.exec_raw((), |state| {
            let status = mlua::ffi::lua_load(
                state,
                read_lua_source_chunk,
                reader_data,
                name.as_ptr(),
                c"t".as_ptr(),
            );
            if status != mlua::ffi::LUA_OK {
                mlua::ffi::lua_error(state);
            }
        })
    };
    if reader.cancelled || cancellation.is_cancelled() {
        return Err(ProjectLuaFailure::Cancelled);
    }
    result.map_err(|error| ProjectLuaFailure::Compile(error.to_string()))
}

fn validate_lua_source_utf8(
    source: &[u8],
    cancellation: &ProjectLuaCancellation,
) -> Result<(), ProjectLuaFailure> {
    if cancellation.is_cancelled() {
        return Err(ProjectLuaFailure::Cancelled);
    }

    let mut pending = Vec::with_capacity(PROJECT_LUA_SOURCE_CHUNK_BYTES.get() + 3);
    for chunk in source.chunks(PROJECT_LUA_SOURCE_CHUNK_BYTES.get()) {
        if cancellation.is_cancelled() {
            return Err(ProjectLuaFailure::Cancelled);
        }
        pending.extend_from_slice(chunk);
        match std::str::from_utf8(&pending) {
            Ok(_) => pending.clear(),
            Err(error) if error.error_len().is_none() => {
                let valid_up_to = error.valid_up_to();
                pending.copy_within(valid_up_to.., 0);
                pending.truncate(pending.len() - valid_up_to);
            }
            Err(_) => {
                return Err(ProjectLuaFailure::Compile(
                    "Lua 主程序必须是有效 UTF-8".to_owned(),
                ));
            }
        }
    }
    if cancellation.is_cancelled() {
        return Err(ProjectLuaFailure::Cancelled);
    }
    if pending.is_empty() {
        Ok(())
    } else {
        Err(ProjectLuaFailure::Compile(
            "Lua 主程序必须是有效 UTF-8".to_owned(),
        ))
    }
}

fn install_arguments(lua: &Lua, request: &ProjectLuaRunRequest) -> mlua::Result<()> {
    let arguments = lua.create_table_with_capacity(request.program.arguments().len(), 1)?;
    arguments.raw_set(0, request.program.identity())?;
    for (index, argument) in request.program.arguments().iter().enumerate() {
        arguments.raw_set(index + 1, argument.as_str())?;
    }
    lua.globals().set("arg", arguments)
}

fn remove_external_capabilities(lua: &Lua) -> Result<(), ProjectLuaFailure> {
    let globals = lua.globals();
    for name in [
        "io", "os", "package", "require", "loadfile", "dofile", "debug", "warn",
    ] {
        globals
            .raw_set(name, Value::Nil)
            .map_err(|error| ProjectLuaFailure::Context(error.to_string()))?;
    }
    Ok(())
}

fn build_context(
    lua: &Lua,
    request: &ProjectLuaRunRequest,
    connection: Rc<RefCell<Connection>>,
    metrics: Arc<BindingMetrics>,
    transaction_guard: Rc<BindingTransactionGuard>,
) -> mlua::Result<Table> {
    let context = lua.create_table()?;
    context.set(
        "project",
        lua.create_userdata(LuaProject {
            name: request.project.name().to_owned(),
            engine: request.project.engine().to_owned(),
        })?,
    )?;

    context.set(
        "db",
        build_database_table(
            lua,
            Rc::clone(&connection),
            Arc::clone(&metrics),
            Rc::clone(&transaction_guard),
            request.cancellation.clone(),
        )?,
    )?;
    context.set(
        "translation",
        build_translation_table(
            lua,
            connection,
            Arc::clone(&request.adapter),
            metrics,
            transaction_guard,
            request.cancellation.clone(),
        )?,
    )?;
    Ok(context)
}

fn install_print(
    lua: &Lua,
    sink: Arc<dyn ProjectLuaPrintSink>,
    metrics: Arc<BindingMetrics>,
    cancellation: ProjectLuaCancellation,
) -> mlua::Result<()> {
    let native = lua.create_function(move |lua, bytes: mlua::LuaString| {
        metrics.record_printed_line();
        ensure_lua_not_cancelled(&cancellation)?;
        let result = sink
            .print(bytes.as_bytes().as_ref())
            .map_err(|error| host_error("log", error, "print"));
        ensure_lua_not_cancelled(&cancellation)?;
        host_result_to_lua(lua, result, |_lua, ()| Ok(Value::Nil))
    })?;
    let factory: Function = lua
        .load(
            r##"
return function(native)
  local tostring = tostring
  local select = select
  local concat = table.concat
  return function(...)
    local values = {}
    for index = 1, select("#", ...) do
      values[index] = tostring(select(index, ...))
    end
    local ok, value = native(concat(values, "\t"))
    if not ok then error(value, 0) end
    return value
  end
end
"##,
        )
        .eval()?;
    let print: Function = factory.call(native)?;
    lua.globals().set("print", print)
}

fn build_database_table(
    lua: &Lua,
    connection: Rc<RefCell<Connection>>,
    metrics: Arc<BindingMetrics>,
    transaction_guard: Rc<BindingTransactionGuard>,
    cancellation: ProjectLuaCancellation,
) -> mlua::Result<Table> {
    let native = lua.create_table()?;
    let null = lua.create_userdata(LuaSqliteNull)?;

    let query_connection = Rc::clone(&connection);
    let query_metrics = Arc::clone(&metrics);
    let query_transaction_guard = Rc::clone(&transaction_guard);
    let query_cancellation = cancellation.clone();
    native.set(
        "query",
        lua.create_function(move |lua, (statement, parameters): (Value, Value)| {
            query_metrics.record_database_call();
            ensure_lua_not_cancelled(&query_cancellation)?;
            let connection = query_connection.borrow();
            let result = query_transaction_guard.call(&connection, "db.query", |connection| {
                parse_sql_call(statement, parameters, &query_cancellation)
                    .map_err(|error| host_error("binding", error, "db.query"))
                    .and_then(|(statement, parameters)| {
                        query_database(connection, &statement, &parameters, &query_cancellation)
                            .map_err(|error| match error {
                                DatabaseQueryError::Sqlite(error) => {
                                    sqlite_host_error("db.query", &error)
                                }
                                DatabaseQueryError::Binding(error) => {
                                    host_error("binding", error, "db.query")
                                }
                            })
                    })
            });
            let output = host_result_to_lua(lua, result, |lua, rows| {
                rows_to_lua(lua, rows, &query_cancellation)
            });
            ensure_lua_not_cancelled(&query_cancellation)?;
            output
        })?,
    )?;

    let execute_metrics = Arc::clone(&metrics);
    let execute_transaction_guard = transaction_guard;
    let execute_cancellation = cancellation.clone();
    native.set(
        "execute",
        lua.create_function(move |lua, (statement, parameters): (Value, Value)| {
            execute_metrics.record_database_call();
            ensure_lua_not_cancelled(&execute_cancellation)?;
            let connection = connection.borrow();
            let result = execute_transaction_guard.call(&connection, "db.execute", |connection| {
                parse_sql_call(statement, parameters, &execute_cancellation)
                    .map_err(|error| host_error("binding", error, "db.execute"))
                    .and_then(|(statement, parameters)| {
                        ensure_project_lua_call_running(&execute_cancellation)
                            .map_err(|error| host_error("binding", error, "db.execute"))?;
                        execute_database(connection, &statement, &parameters)
                            .map_err(|error| sqlite_host_error("db.execute", &error))
                    })
            });
            let output = match result {
                Ok(changed) => {
                    execute_metrics.record_changed_rows(changed);
                    host_result_to_lua(lua, Ok(changed), |_lua, changed| {
                        i64::try_from(changed)
                            .map(Value::Integer)
                            .map_err(|_| mlua::Error::runtime("SQLite 受影响行数超出 Lua integer"))
                    })
                }
                Err(error) => {
                    host_result_to_lua::<u64, _>(lua, Err(error), |_lua, _changed| Ok(Value::Nil))
                }
            };
            ensure_lua_not_cancelled(&execute_cancellation)?;
            output
        })?,
    )?;

    let blob_cancellation = cancellation;
    let blob = lua.create_function(move |lua, value: Value| {
        ensure_lua_not_cancelled(&blob_cancellation)?;
        let result = match value {
            Value::String(bytes) => {
                clone_bytes_with_cancellation(bytes.as_bytes().as_ref(), &blob_cancellation)
                    .map_err(|error| host_error("binding", error, "db.blob"))
            }
            other => Err(LuaHostCallError::binding(
                "db.blob",
                format!(
                    "ctx.db.blob 的参数必须是字符串，实际为 {}",
                    other.type_name()
                ),
            )),
        };
        let output = host_result_to_lua(lua, result, |lua, bytes| {
            lua.create_userdata(LuaBlob {
                bytes,
                cancellation: blob_cancellation.clone(),
            })
            .map(Value::UserData)
        });
        ensure_lua_not_cancelled(&blob_cancellation)?;
        output
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
  }
end
"#,
        )
        .eval()?;
    factory.call((native, null, blob))
}

fn build_translation_table(
    lua: &Lua,
    connection: Rc<RefCell<Connection>>,
    adapter: Arc<dyn ProjectLuaEngineAdapter>,
    metrics: Arc<BindingMetrics>,
    transaction_guard: Rc<BindingTransactionGuard>,
    cancellation: ProjectLuaCancellation,
) -> mlua::Result<Table> {
    let native = lua.create_table()?;

    let set_adapter = Arc::clone(&adapter);
    let set_connection = Rc::clone(&connection);
    let set_metrics = Arc::clone(&metrics);
    let set_transaction_guard = Rc::clone(&transaction_guard);
    let set_cancellation = cancellation.clone();
    native.set(
        "set",
        lua.create_function(move |lua, (locator, translation): (Value, Value)| {
            set_metrics.record_translation_call();
            ensure_lua_not_cancelled(&set_cancellation)?;
            let connection = set_connection.borrow();
            let result = set_transaction_guard.call(&connection, "translation.set", |connection| {
                lua_to_project_value(locator, &set_cancellation)
                    .and_then(|locator| {
                        lua_to_project_value(translation, &set_cancellation)
                            .map(|translation| (locator, translation))
                    })
                    .map_err(|error| host_error("binding", error, "translation.set"))
                    .and_then(|(locator, translation)| {
                        set_adapter
                            .set_translation(connection, locator, translation)
                            .map_err(|error| host_error("translation", error, "translation.set"))
                    })
            });
            let output = match result {
                Ok(changed) => {
                    set_metrics.record_changed_rows(changed);
                    host_result_to_lua(lua, Ok(()), |_lua, ()| Ok(Value::Nil))
                }
                Err(error) => host_result_to_lua(lua, Err(error), |_lua, ()| Ok(Value::Nil)),
            };
            ensure_lua_not_cancelled(&set_cancellation)?;
            output
        })?,
    )?;

    let clear_metrics = metrics;
    let clear_transaction_guard = transaction_guard;
    let clear_cancellation = cancellation;
    native.set(
        "clear",
        lua.create_function(move |lua, locator: Value| {
            clear_metrics.record_translation_call();
            ensure_lua_not_cancelled(&clear_cancellation)?;
            let connection = connection.borrow();
            let result =
                clear_transaction_guard.call(&connection, "translation.clear", |connection| {
                    lua_to_project_value(locator, &clear_cancellation)
                        .map_err(|error| host_error("binding", error, "translation.clear"))
                        .and_then(|locator| {
                            adapter
                                .clear_translation(connection, locator)
                                .map_err(|error| {
                                    host_error("translation", error, "translation.clear")
                                })
                        })
                });
            let output = match result {
                Ok(changed) => {
                    clear_metrics.record_changed_rows(changed);
                    host_result_to_lua(lua, Ok(()), |_lua, ()| Ok(Value::Nil))
                }
                Err(error) => host_result_to_lua(lua, Err(error), |_lua, ()| Ok(Value::Nil)),
            };
            ensure_lua_not_cancelled(&clear_cancellation)?;
            output
        })?,
    )?;

    checked_function_table(lua, native, &["set", "clear"])
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

fn parse_sql_call(
    statement: Value,
    parameters: Value,
    cancellation: &ProjectLuaCancellation,
) -> Result<(String, Vec<rusqlite::types::Value>), ProjectLuaCallError> {
    ensure_project_lua_call_running(cancellation)?;
    let statement = match statement {
        Value::String(statement) => strict_text(&statement, "SQL", cancellation)?,
        value => {
            return Err(ProjectLuaCallError::new(
                "invalid_statement",
                format!("SQL 必须是字符串，实际为 {}", value.type_name()),
            ));
        }
    };
    let parameters = match parameters {
        Value::Nil => Vec::new(),
        Value::Table(table) => {
            let values = dense_table_values(table, cancellation)?;
            let mut parameters = Vec::with_capacity(values.len());
            for value in values {
                ensure_project_lua_call_running(cancellation)?;
                parameters.push(lua_to_sqlite_value(value, cancellation)?);
            }
            parameters
        }
        other => {
            return Err(ProjectLuaCallError::new(
                "invalid_parameters",
                format!(
                    "SQLite parameters 必须是无洞数组或 nil，实际为 {}",
                    other.type_name()
                ),
            ));
        }
    };
    ensure_project_lua_call_running(cancellation)?;
    Ok((statement, parameters))
}

fn lua_to_sqlite_value(
    value: Value,
    cancellation: &ProjectLuaCancellation,
) -> Result<rusqlite::types::Value, ProjectLuaCallError> {
    ensure_project_lua_call_running(cancellation)?;
    let converted = match value {
        Value::Integer(value) => Ok(rusqlite::types::Value::Integer(value)),
        Value::Number(value) if value.is_finite() => Ok(rusqlite::types::Value::Real(value)),
        Value::Number(_) => Err(ProjectLuaCallError::new(
            "invalid_real",
            "SQLite REAL 参数不得为 NaN 或 Inf",
        )),
        Value::String(value) => {
            strict_text(&value, "SQLite TEXT", cancellation).map(rusqlite::types::Value::Text)
        }
        Value::UserData(value) if value.is::<LuaSqliteNull>() => Ok(rusqlite::types::Value::Null),
        Value::UserData(value) if value.is::<LuaBlob>() => {
            let value = value
                .borrow::<LuaBlob>()
                .map_err(|error| ProjectLuaCallError::new("invalid_blob", error.to_string()))?;
            clone_bytes_with_cancellation(&value.bytes, cancellation)
                .map(rusqlite::types::Value::Blob)
        }
        other => Err(ProjectLuaCallError::new(
            "unsupported_parameter",
            format!("SQLite 参数不支持 {}", other.type_name()),
        )),
    };
    ensure_project_lua_call_running(cancellation)?;
    converted
}

#[derive(Debug)]
enum DatabaseQueryError {
    Sqlite(rusqlite::Error),
    Binding(ProjectLuaCallError),
}

impl From<rusqlite::Error> for DatabaseQueryError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl From<ProjectLuaCallError> for DatabaseQueryError {
    fn from(error: ProjectLuaCallError) -> Self {
        Self::Binding(error)
    }
}

fn query_database(
    connection: &Connection,
    sql: &str,
    parameters: &[rusqlite::types::Value],
    cancellation: &ProjectLuaCancellation,
) -> Result<Vec<Vec<rusqlite::types::Value>>, DatabaseQueryError> {
    ensure_project_lua_call_running(cancellation)?;
    let mut statement = connection.prepare(sql)?;
    ensure_project_lua_call_running(cancellation)?;
    if !statement.readonly() || statement.column_count() == 0 {
        return Err(DatabaseQueryError::Sqlite(rusqlite::Error::InvalidQuery));
    }
    let column_count = statement.column_count();
    let mut rows = statement.query(params_from_iter(parameters.iter()))?;
    ensure_project_lua_call_running(cancellation)?;
    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        ensure_project_lua_call_running(cancellation)?;
        let mut values = Vec::with_capacity(column_count);
        for index in 0..column_count {
            ensure_project_lua_call_running(cancellation)?;
            values.push(owned_sqlite_value(row.get_ref(index)?, cancellation)?);
        }
        result.push(values);
    }
    ensure_project_lua_call_running(cancellation)?;
    Ok(result)
}

fn execute_database(
    connection: &Connection,
    sql: &str,
    parameters: &[rusqlite::types::Value],
) -> rusqlite::Result<u64> {
    let mut statement = connection.prepare(sql)?;
    if statement.column_count() != 0 {
        return Err(rusqlite::Error::ExecuteReturnedResults);
    }
    let total_before = connection.total_changes();
    let direct = statement.execute(params_from_iter(parameters.iter()))?;
    let total_delta = connection.total_changes().saturating_sub(total_before);
    Ok(u64::try_from(direct)
        .expect("SQLite direct changed rows 必须能表示为 u64")
        .min(total_delta))
}

fn owned_sqlite_value(
    value: ValueRef<'_>,
    cancellation: &ProjectLuaCancellation,
) -> Result<rusqlite::types::Value, DatabaseQueryError> {
    ensure_project_lua_call_running(cancellation)?;
    match value {
        ValueRef::Null => Ok(rusqlite::types::Value::Null),
        ValueRef::Integer(value) => Ok(rusqlite::types::Value::Integer(value)),
        ValueRef::Real(value) => Ok(rusqlite::types::Value::Real(value)),
        ValueRef::Text(value) => match clone_utf8_text_with_cancellation(value, cancellation)? {
            Ok(value) => Ok(rusqlite::types::Value::Text(value)),
            Err(error) => Err(DatabaseQueryError::Sqlite(
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                ),
            )),
        },
        ValueRef::Blob(value) => clone_bytes_with_cancellation(value, cancellation)
            .map(rusqlite::types::Value::Blob)
            .map_err(DatabaseQueryError::Binding),
    }
}

fn rows_to_lua(
    lua: &Lua,
    rows: Vec<Vec<rusqlite::types::Value>>,
    cancellation: &ProjectLuaCancellation,
) -> mlua::Result<Value> {
    ensure_lua_not_cancelled(cancellation)?;
    let result = lua.create_table_with_capacity(rows.len(), 0);
    ensure_lua_not_cancelled(cancellation)?;
    let result = result?;
    for (row_index, row) in rows.into_iter().enumerate() {
        ensure_lua_not_cancelled(cancellation)?;
        let values = lua.create_table_with_capacity(row.len(), 0);
        ensure_lua_not_cancelled(cancellation)?;
        let values = values?;
        for (column_index, value) in row.into_iter().enumerate() {
            ensure_lua_not_cancelled(cancellation)?;
            let value = sqlite_to_lua_value(lua, value, cancellation)?;
            let set_result = values.raw_set(column_index + 1, value);
            ensure_lua_not_cancelled(cancellation)?;
            set_result?;
        }
        let set_result = result.raw_set(row_index + 1, values);
        ensure_lua_not_cancelled(cancellation)?;
        set_result?;
    }
    ensure_lua_not_cancelled(cancellation)?;
    Ok(Value::Table(result))
}

fn sqlite_to_lua_value(
    lua: &Lua,
    value: rusqlite::types::Value,
    cancellation: &ProjectLuaCancellation,
) -> mlua::Result<Value> {
    ensure_lua_not_cancelled(cancellation)?;
    let converted = match value {
        rusqlite::types::Value::Null => lua.create_userdata(LuaSqliteNull).map(Value::UserData),
        rusqlite::types::Value::Integer(value) => Ok(Value::Integer(value)),
        rusqlite::types::Value::Real(value) if value.is_finite() => Ok(Value::Number(value)),
        rusqlite::types::Value::Real(_) => {
            Err(mlua::Error::runtime("SQLite REAL 结果为 NaN 或 Inf"))
        }
        rusqlite::types::Value::Text(value) => lua.create_string(value).map(Value::String),
        rusqlite::types::Value::Blob(value) => lua
            .create_userdata(LuaBlob {
                bytes: value,
                cancellation: cancellation.clone(),
            })
            .map(Value::UserData),
    };
    ensure_lua_not_cancelled(cancellation)?;
    converted
}

fn dense_table_values(
    table: Table,
    cancellation: &ProjectLuaCancellation,
) -> Result<Vec<Value>, ProjectLuaCallError> {
    ensure_project_lua_call_running(cancellation)?;
    let mut indexed = BTreeMap::new();
    for pair in table.pairs::<Value, Value>() {
        ensure_project_lua_call_running(cancellation)?;
        let (key, value) =
            pair.map_err(|error| ProjectLuaCallError::new("invalid_array", error.to_string()))?;
        let Value::Integer(index) = key else {
            return Err(ProjectLuaCallError::new(
                "invalid_array",
                "数组不得包含非整数键",
            ));
        };
        let index = usize::try_from(index)
            .ok()
            .filter(|index| *index > 0)
            .ok_or_else(|| ProjectLuaCallError::new("invalid_array", "数组下标必须从 1 开始"))?;
        if indexed.insert(index, value).is_some() {
            return Err(ProjectLuaCallError::new(
                "invalid_array",
                "数组必须无洞且连续",
            ));
        }
    }
    let mut values = Vec::with_capacity(indexed.len());
    for (offset, (index, value)) in indexed.into_iter().enumerate() {
        ensure_project_lua_call_running(cancellation)?;
        if index != offset + 1 {
            return Err(ProjectLuaCallError::new(
                "invalid_array",
                "数组必须无洞且连续",
            ));
        }
        values.push(value);
    }
    ensure_project_lua_call_running(cancellation)?;
    Ok(values)
}

fn lua_to_project_value(
    value: Value,
    cancellation: &ProjectLuaCancellation,
) -> Result<ProjectLuaValue, ProjectLuaCallError> {
    let mut checkpoint = || {};
    lua_to_project_value_with_checkpoint(value, cancellation, &mut checkpoint)
}

struct ProjectTableConversionFrame {
    identity: *const c_void,
    pairs: std::vec::IntoIter<(Value, Value)>,
    pending_key: Option<Value>,
    integer_fields: BTreeMap<usize, ProjectLuaValue>,
    string_fields: Vec<(String, ProjectLuaValue)>,
}

fn lua_to_project_value_with_checkpoint(
    value: Value,
    cancellation: &ProjectLuaCancellation,
    checkpoint: &mut impl FnMut(),
) -> Result<ProjectLuaValue, ProjectLuaCallError> {
    let mut active_tables = HashSet::new();
    let mut frames = Vec::<ProjectTableConversionFrame>::new();
    let mut next_value = Some(value);
    let mut completed_value = None;

    loop {
        project_value_conversion_checkpoint(cancellation, checkpoint)?;

        if let Some(value) = next_value.take() {
            match value {
                Value::Nil => completed_value = Some(ProjectLuaValue::Nil),
                Value::Boolean(value) => {
                    completed_value = Some(ProjectLuaValue::Boolean(value));
                }
                Value::Integer(value) => {
                    completed_value = Some(ProjectLuaValue::Integer(value));
                }
                Value::Number(value) if value.is_finite() => {
                    completed_value = Some(ProjectLuaValue::Real(value));
                }
                Value::Number(_) => {
                    return Err(ProjectLuaCallError::new(
                        "invalid_real",
                        "translation 参数不得包含 NaN 或 Inf",
                    ));
                }
                Value::String(value) => {
                    completed_value = Some(ProjectLuaValue::Text(strict_text(
                        &value,
                        "translation 字符串",
                        cancellation,
                    )?));
                }
                Value::UserData(value) if value.is::<LuaSqliteNull>() => {
                    completed_value = Some(ProjectLuaValue::Nil);
                }
                Value::UserData(value) if value.is::<LuaBlob>() => {
                    let value = value.borrow::<LuaBlob>().map_err(|error| {
                        ProjectLuaCallError::new("invalid_blob", error.to_string())
                    })?;
                    completed_value = Some(ProjectLuaValue::Blob(clone_bytes_with_cancellation(
                        &value.bytes,
                        cancellation,
                    )?));
                }
                Value::Table(table) => {
                    let identity = table.to_pointer();
                    if !active_tables.insert(identity) {
                        return Err(ProjectLuaCallError::new(
                            "cyclic_table",
                            "translation 参数不得包含循环 table",
                        ));
                    }
                    let pairs =
                        collect_project_table_pairs(table, cancellation, checkpoint)?.into_iter();
                    frames.push(ProjectTableConversionFrame {
                        identity,
                        pairs,
                        pending_key: None,
                        integer_fields: BTreeMap::new(),
                        string_fields: Vec::new(),
                    });
                }
                other => {
                    return Err(ProjectLuaCallError::new(
                        "unsupported_value",
                        format!("translation 参数不支持 {}", other.type_name()),
                    ));
                }
            }
            continue;
        }

        if let Some(value) = completed_value.take() {
            let Some(frame) = frames.last_mut() else {
                project_value_conversion_checkpoint(cancellation, checkpoint)?;
                return Ok(value);
            };
            let key = frame
                .pending_key
                .take()
                .expect("子值完成时父 table 必须保存对应键");
            insert_project_table_field(frame, key, value, cancellation, checkpoint)?;
            continue;
        }

        let frame = frames.last_mut().expect("未完成转换时必须存在 table frame");
        if let Some((key, value)) = frame.pairs.next() {
            frame.pending_key = Some(key);
            next_value = Some(value);
            continue;
        }

        let frame = frames.pop().expect("刚确认存在 table frame");
        active_tables.remove(&frame.identity);
        completed_value = Some(finish_project_table(frame, cancellation, checkpoint)?);
    }
}

fn collect_project_table_pairs(
    table: Table,
    cancellation: &ProjectLuaCancellation,
    checkpoint: &mut impl FnMut(),
) -> Result<Vec<(Value, Value)>, ProjectLuaCallError> {
    let mut pairs = Vec::new();
    for pair in table.pairs::<Value, Value>() {
        project_value_conversion_checkpoint(cancellation, checkpoint)?;
        pairs.push(
            pair.map_err(|error| ProjectLuaCallError::new("invalid_table", error.to_string()))?,
        );
    }
    project_value_conversion_checkpoint(cancellation, checkpoint)?;
    Ok(pairs)
}

fn insert_project_table_field(
    frame: &mut ProjectTableConversionFrame,
    key: Value,
    value: ProjectLuaValue,
    cancellation: &ProjectLuaCancellation,
    checkpoint: &mut impl FnMut(),
) -> Result<(), ProjectLuaCallError> {
    project_value_conversion_checkpoint(cancellation, checkpoint)?;
    match key {
        Value::Integer(index) => {
            let index = usize::try_from(index)
                .ok()
                .filter(|index| *index > 0)
                .ok_or_else(|| {
                    ProjectLuaCallError::new("invalid_table", "数组下标必须从 1 开始")
                })?;
            if frame.integer_fields.insert(index, value).is_some() {
                return Err(ProjectLuaCallError::new(
                    "duplicate_field",
                    "translation table 包含重复字段",
                ));
            }
        }
        Value::String(key) => {
            let key = strict_text(&key, "translation 字段名", cancellation)?;
            frame.string_fields.push((key, value));
        }
        other => {
            return Err(ProjectLuaCallError::new(
                "invalid_table",
                format!(
                    "translation table 只允许正整数或 UTF-8 字符串键，实际为 {}",
                    other.type_name()
                ),
            ));
        }
    }
    project_value_conversion_checkpoint(cancellation, checkpoint)
}

fn finish_project_table(
    frame: ProjectTableConversionFrame,
    cancellation: &ProjectLuaCancellation,
    checkpoint: &mut impl FnMut(),
) -> Result<ProjectLuaValue, ProjectLuaCallError> {
    let string_fields = sort_project_object_fields(frame.string_fields, cancellation, checkpoint)?;
    if !frame.integer_fields.is_empty() && !string_fields.is_empty() {
        return Err(ProjectLuaCallError::new(
            "mixed_table",
            "translation table 不能混用数组下标和字段名",
        ));
    }
    if !string_fields.is_empty() || frame.integer_fields.is_empty() {
        return Ok(ProjectLuaValue::Object(string_fields));
    }

    let mut values = Vec::with_capacity(frame.integer_fields.len());
    for (offset, (index, value)) in frame.integer_fields.into_iter().enumerate() {
        project_value_conversion_checkpoint(cancellation, checkpoint)?;
        if index != offset + 1 {
            return Err(ProjectLuaCallError::new(
                "invalid_array",
                "translation 数组必须无洞且连续",
            ));
        }
        values.push(value);
    }
    project_value_conversion_checkpoint(cancellation, checkpoint)?;
    Ok(ProjectLuaValue::Array(values))
}

fn sort_project_object_fields(
    fields: Vec<(String, ProjectLuaValue)>,
    cancellation: &ProjectLuaCancellation,
    checkpoint: &mut impl FnMut(),
) -> Result<Vec<(String, ProjectLuaValue)>, ProjectLuaCallError> {
    if fields.len() < 2 {
        return Ok(fields);
    }

    let mut order = Vec::with_capacity(fields.len());
    for index in 0..fields.len() {
        project_value_conversion_checkpoint(cancellation, checkpoint)?;
        order.push(index);
    }
    let mut scratch = Vec::with_capacity(fields.len());
    let mut width = 1_usize;
    while width < order.len() {
        scratch.clear();
        let mut start = 0;
        while start < order.len() {
            project_value_conversion_checkpoint(cancellation, checkpoint)?;
            let middle = start.saturating_add(width).min(order.len());
            let end = start
                .saturating_add(width.saturating_mul(2))
                .min(order.len());
            let mut left = start;
            let mut right = middle;
            while left < middle && right < end {
                project_value_conversion_checkpoint(cancellation, checkpoint)?;
                let ordering = compare_project_field_names(
                    &fields[order[left]].0,
                    &fields[order[right]].0,
                    cancellation,
                    checkpoint,
                )?;
                if ordering == CmpOrdering::Greater {
                    scratch.push(order[right]);
                    right += 1;
                } else {
                    scratch.push(order[left]);
                    left += 1;
                }
            }
            while left < middle {
                project_value_conversion_checkpoint(cancellation, checkpoint)?;
                scratch.push(order[left]);
                left += 1;
            }
            while right < end {
                project_value_conversion_checkpoint(cancellation, checkpoint)?;
                scratch.push(order[right]);
                right += 1;
            }
            start = end;
        }
        std::mem::swap(&mut order, &mut scratch);
        width = width.checked_mul(2).unwrap_or(order.len());
    }

    for adjacent in order.windows(2) {
        project_value_conversion_checkpoint(cancellation, checkpoint)?;
        if compare_project_field_names(
            &fields[adjacent[0]].0,
            &fields[adjacent[1]].0,
            cancellation,
            checkpoint,
        )? == CmpOrdering::Equal
        {
            return Err(ProjectLuaCallError::new(
                "duplicate_field",
                "translation table 包含重复字段",
            ));
        }
    }

    let mut slots = Vec::with_capacity(fields.len());
    for field in fields {
        project_value_conversion_checkpoint(cancellation, checkpoint)?;
        slots.push(Some(field));
    }
    let mut sorted = Vec::with_capacity(slots.len());
    for index in order {
        project_value_conversion_checkpoint(cancellation, checkpoint)?;
        sorted.push(slots[index].take().expect("排序索引必须唯一且有效"));
    }
    project_value_conversion_checkpoint(cancellation, checkpoint)?;
    Ok(sorted)
}

fn compare_project_field_names(
    left: &str,
    right: &str,
    cancellation: &ProjectLuaCancellation,
    checkpoint: &mut impl FnMut(),
) -> Result<CmpOrdering, ProjectLuaCallError> {
    let shared_len = left.len().min(right.len());
    let mut start = 0;
    while start < shared_len {
        project_value_conversion_checkpoint(cancellation, checkpoint)?;
        let end = start
            .saturating_add(PROJECT_LUA_SOURCE_CHUNK_BYTES.get())
            .min(shared_len);
        let ordering = left.as_bytes()[start..end].cmp(&right.as_bytes()[start..end]);
        if ordering != CmpOrdering::Equal {
            return Ok(ordering);
        }
        start = end;
    }
    project_value_conversion_checkpoint(cancellation, checkpoint)?;
    Ok(left.len().cmp(&right.len()))
}

fn project_value_conversion_checkpoint(
    cancellation: &ProjectLuaCancellation,
    checkpoint: &mut impl FnMut(),
) -> Result<(), ProjectLuaCallError> {
    ensure_project_lua_call_running(cancellation)?;
    checkpoint();
    ensure_project_lua_call_running(cancellation)
}

fn ensure_project_lua_call_running(
    cancellation: &ProjectLuaCancellation,
) -> Result<(), ProjectLuaCallError> {
    if cancellation.is_cancelled() {
        Err(ProjectLuaCallError::new("cancelled", "ATT 中止了 Lua 脚本"))
    } else {
        Ok(())
    }
}

fn clone_bytes_with_cancellation(
    bytes: &[u8],
    cancellation: &ProjectLuaCancellation,
) -> Result<Vec<u8>, ProjectLuaCallError> {
    ensure_project_lua_call_running(cancellation)?;
    let mut cloned = Vec::with_capacity(bytes.len());
    for chunk in bytes.chunks(PROJECT_LUA_SOURCE_CHUNK_BYTES.get()) {
        ensure_project_lua_call_running(cancellation)?;
        cloned.extend_from_slice(chunk);
    }
    ensure_project_lua_call_running(cancellation)?;
    Ok(cloned)
}

fn clone_utf8_text_with_cancellation(
    bytes: &[u8],
    cancellation: &ProjectLuaCancellation,
) -> Result<Result<String, std::str::Utf8Error>, ProjectLuaCallError> {
    let mut checkpoint = || {};
    clone_utf8_text_with_checkpoint(bytes, cancellation, &mut checkpoint)
}

fn clone_utf8_text_with_checkpoint(
    bytes: &[u8],
    cancellation: &ProjectLuaCancellation,
    checkpoint: &mut impl FnMut(),
) -> Result<Result<String, std::str::Utf8Error>, ProjectLuaCallError> {
    ensure_project_lua_call_running(cancellation)?;
    let mut text = String::with_capacity(bytes.len());
    let mut pending = Vec::with_capacity(PROJECT_LUA_SOURCE_CHUNK_BYTES.get() + 3);
    for chunk in bytes.chunks(PROJECT_LUA_SOURCE_CHUNK_BYTES.get()) {
        ensure_project_lua_call_running(cancellation)?;
        checkpoint();
        ensure_project_lua_call_running(cancellation)?;
        pending.extend_from_slice(chunk);
        match std::str::from_utf8(&pending) {
            Ok(valid) => {
                text.push_str(valid);
                pending.clear();
            }
            Err(error) if error.error_len().is_none() => {
                let valid_up_to = error.valid_up_to();
                let valid = std::str::from_utf8(&pending[..valid_up_to])
                    .expect("Utf8Error::valid_up_to 指向有效 UTF-8 前缀");
                text.push_str(valid);
                pending.copy_within(valid_up_to.., 0);
                pending.truncate(pending.len() - valid_up_to);
            }
            Err(error) => return Ok(Err(error)),
        }
        ensure_project_lua_call_running(cancellation)?;
    }
    if !pending.is_empty() {
        return Ok(Err(
            std::str::from_utf8(&pending).expect_err("pending 只保留不完整 UTF-8 后缀")
        ));
    }
    ensure_project_lua_call_running(cancellation)?;
    Ok(Ok(text))
}

fn strict_text(
    value: &mlua::LuaString,
    role: &str,
    cancellation: &ProjectLuaCancellation,
) -> Result<String, ProjectLuaCallError> {
    match clone_utf8_text_with_cancellation(value.as_bytes().as_ref(), cancellation)? {
        Ok(value) => Ok(value),
        Err(_) => Err(ProjectLuaCallError::new(
            "invalid_utf8",
            format!("{role} 必须是 UTF-8 字符串"),
        )),
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
struct LuaProject {
    name: String,
    engine: String,
}

impl UserData for LuaProject {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("name", |_lua, this| Ok(this.name.clone()));
        fields.add_field_method_get("engine", |_lua, this| Ok(this.engine.clone()));
    }
}

#[derive(Clone, Debug)]
struct LuaBlob {
    bytes: Vec<u8>,
    cancellation: ProjectLuaCancellation,
}

impl UserData for LuaBlob {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("bytes", |lua, this, ()| {
            ensure_lua_not_cancelled(&this.cancellation)?;
            let value = lua.create_string(&this.bytes);
            ensure_lua_not_cancelled(&this.cancellation)?;
            value
        });
        methods.add_meta_method(MetaMethod::Eq, |_lua, this, other: AnyUserData| {
            ensure_lua_not_cancelled(&this.cancellation)?;
            if !other.is::<LuaBlob>() {
                return Ok(false);
            }
            let other = other.borrow::<LuaBlob>()?;
            ensure_lua_not_cancelled(&other.cancellation)?;
            if other.bytes.len() != this.bytes.len() {
                return Ok(false);
            }
            for (left, right) in this
                .bytes
                .chunks(PROJECT_LUA_SOURCE_CHUNK_BYTES.get())
                .zip(other.bytes.chunks(PROJECT_LUA_SOURCE_CHUNK_BYTES.get()))
            {
                ensure_lua_not_cancelled(&this.cancellation)?;
                ensure_lua_not_cancelled(&other.cancellation)?;
                if left != right {
                    return Ok(false);
                }
            }
            ensure_lua_not_cancelled(&this.cancellation)?;
            ensure_lua_not_cancelled(&other.cancellation)?;
            Ok(true)
        });
    }
}

#[derive(Clone, Debug)]
struct LuaHostCallError {
    domain: &'static str,
    kind: &'static str,
    operation: &'static str,
    message: String,
}

impl LuaHostCallError {
    fn binding(operation: &'static str, message: impl Into<String>) -> Self {
        Self {
            domain: "binding",
            kind: "invalid_value",
            operation,
            message: message.into(),
        }
    }
}

impl UserData for LuaHostCallError {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("domain", |_lua, this| Ok(this.domain));
        fields.add_field_method_get("kind", |_lua, this| Ok(this.kind));
        fields.add_field_method_get("operation", |_lua, this| Ok(this.operation));
        fields.add_field_method_get("message", |_lua, this| Ok(this.message.clone()));
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::ToString, |_lua, this, ()| {
            Ok(this.message.clone())
        });
    }
}

fn host_error(
    domain: &'static str,
    error: ProjectLuaCallError,
    operation: &'static str,
) -> LuaHostCallError {
    LuaHostCallError {
        domain,
        kind: error.kind(),
        operation,
        message: error.message().to_owned(),
    }
}

fn transaction_lost_error(operation: &'static str) -> LuaHostCallError {
    LuaHostCallError {
        domain: "database",
        kind: "transaction_lost",
        operation,
        message: "SQL 提前结束了 ATT 外层事务；本次 Lua 的全部数据库修改均不提交".to_owned(),
    }
}

fn sqlite_host_error(operation: &'static str, error: &rusqlite::Error) -> LuaHostCallError {
    LuaHostCallError {
        domain: "sqlite",
        kind: match error {
            rusqlite::Error::MultipleStatement => "multiple_statements",
            rusqlite::Error::ExecuteReturnedResults => "statement_returns_rows",
            rusqlite::Error::InvalidQuery => "statement_returns_no_rows",
            rusqlite::Error::InvalidParameterCount(_, _) => "invalid_parameter_count",
            rusqlite::Error::SqliteFailure(_, _) => "sqlite_failure",
            _ => "sqlite_driver",
        },
        operation,
        message: error.to_string(),
    }
}

fn host_result_to_lua<T, F>(
    lua: &Lua,
    result: Result<T, LuaHostCallError>,
    success: F,
) -> mlua::Result<MultiValue>
where
    F: FnOnce(&Lua, T) -> mlua::Result<Value>,
{
    match result {
        Ok(value) => match success(lua, value) {
            Ok(value) => Ok(MultiValue::from_vec(vec![Value::Boolean(true), value])),
            Err(error) => Ok(MultiValue::from_vec(vec![
                Value::Boolean(false),
                Value::UserData(lua.create_userdata(LuaHostCallError::binding(
                    "binding.return",
                    error.to_string(),
                ))?),
            ])),
        },
        Err(error) => Ok(MultiValue::from_vec(vec![
            Value::Boolean(false),
            Value::UserData(lua.create_userdata(error)?),
        ])),
    }
}

pub(super) fn execute(
    lua: &Lua,
    function: Function,
    cancellation: &ProjectLuaCancellation,
) -> Result<(), ProjectLuaFailure> {
    let runner: Function = lua
        .load(
            "return function(main) local ok, value = xpcall(main, function(error) return error end); return ok, value end",
        )
        .eval()
        .map_err(|error| ProjectLuaFailure::Context(error.to_string()))?;
    let thread = lua
        .create_thread(runner)
        .map_err(|error| ProjectLuaFailure::Context(error.to_string()))?;
    let result = thread.resume::<(bool, Value)>(function);
    if cancellation.is_cancelled() {
        return Err(ProjectLuaFailure::Cancelled);
    }
    if thread.status() == ThreadStatus::Resumable {
        return Err(ProjectLuaFailure::Script(
            "Lua 主程序不得主动 yield".to_owned(),
        ));
    }
    let (succeeded, error) =
        result.map_err(|error| ProjectLuaFailure::Script(error.to_string()))?;
    if succeeded {
        return Ok(());
    }
    if let Value::UserData(userdata) = &error
        && let Ok(error) = userdata.borrow::<LuaHostCallError>()
    {
        let message = clone_utf8_text_with_cancellation(error.message.as_bytes(), cancellation)
            .map_err(|_| ProjectLuaFailure::Cancelled)?
            .expect("Rust String 必须保持有效 UTF-8");
        if cancellation.is_cancelled() {
            return Err(ProjectLuaFailure::Cancelled);
        }
        return Err(ProjectLuaFailure::Host {
            domain: error.domain,
            kind: error.kind,
            operation: error.operation,
            message,
        });
    }
    Err(ProjectLuaFailure::Script(lua_value_description(
        &error,
        cancellation,
    )?))
}

fn lua_value_description(
    value: &Value,
    cancellation: &ProjectLuaCancellation,
) -> Result<String, ProjectLuaFailure> {
    let mut checkpoint = || {};
    lua_value_description_with_checkpoint(value, cancellation, &mut checkpoint)
}

fn lua_value_description_with_checkpoint(
    value: &Value,
    cancellation: &ProjectLuaCancellation,
    checkpoint: &mut impl FnMut(),
) -> Result<String, ProjectLuaFailure> {
    if cancellation.is_cancelled() {
        return Err(ProjectLuaFailure::Cancelled);
    }
    match value {
        Value::String(value) => {
            match clone_utf8_text_with_checkpoint(
                value.as_bytes().as_ref(),
                cancellation,
                checkpoint,
            )
            .map_err(|_| ProjectLuaFailure::Cancelled)?
            {
                Ok(value) => Ok(value),
                Err(_) => escaped_lua_bytes(value.as_bytes().as_ref(), cancellation, checkpoint),
            }
        }
        Value::Error(error) => {
            let description = error.to_string();
            if cancellation.is_cancelled() {
                Err(ProjectLuaFailure::Cancelled)
            } else {
                Ok(description)
            }
        }
        other => {
            let description = format!("Lua {} 错误值", other.type_name());
            if cancellation.is_cancelled() {
                Err(ProjectLuaFailure::Cancelled)
            } else {
                Ok(description)
            }
        }
    }
}

fn escaped_lua_bytes(
    bytes: &[u8],
    cancellation: &ProjectLuaCancellation,
    checkpoint: &mut impl FnMut(),
) -> Result<String, ProjectLuaFailure> {
    use std::fmt::Write as _;

    let mut escaped = String::from("非 UTF-8 Lua 错误字符串（原始字节）：");
    for chunk in bytes.chunks(PROJECT_LUA_SOURCE_CHUNK_BYTES.get()) {
        if cancellation.is_cancelled() {
            return Err(ProjectLuaFailure::Cancelled);
        }
        checkpoint();
        if cancellation.is_cancelled() {
            return Err(ProjectLuaFailure::Cancelled);
        }
        for byte in chunk {
            write!(&mut escaped, "\\x{byte:02X}").expect("写入 String 不会失败");
        }
    }
    if cancellation.is_cancelled() {
        Err(ProjectLuaFailure::Cancelled)
    } else {
        Ok(escaped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEEP_PROJECT_VALUE_TABLE_DEPTH: usize = 5_000;

    #[test]
    fn cancelled_host_entry_is_still_counted() {
        let lua = Lua::new();
        let cancellation = ProjectLuaCancellation::default();
        let metrics = Arc::new(BindingMetrics::default());
        let database = build_database_table(
            &lua,
            Rc::new(RefCell::new(
                Connection::open_in_memory().expect("应建立测试数据库"),
            )),
            Arc::clone(&metrics),
            Rc::new(BindingTransactionGuard::default()),
            cancellation.clone(),
        )
        .expect("应建立数据库 Host table");
        cancellation.cancel();

        let query: Function = database.get("query").expect("应取得 query Host");
        query
            .call::<Value>(("SELECT 1", Value::Nil))
            .expect_err("进入 Host 后观察到取消必须返回失败");
        assert_eq!(metrics.database_calls(), 1);
        assert_eq!(metrics.changed_rows(), 0);
    }

    #[test]
    fn changed_rows_are_counted_before_post_execute_cancellation() {
        let lua = Lua::new();
        let cancellation = ProjectLuaCancellation::default();
        let cancel_from_authorizer = cancellation.clone();
        let connection = Connection::open_in_memory().expect("应建立测试数据库");
        connection
            .execute_batch(
                "CREATE TABLE units (id TEXT PRIMARY KEY, translation TEXT);
                 INSERT INTO units VALUES ('unit-1', NULL);
                 BEGIN IMMEDIATE;",
            )
            .expect("应建立测试事务");
        connection
            .authorizer(Some(move |context: rusqlite::hooks::AuthContext<'_>| {
                if matches!(context.action, rusqlite::hooks::AuthAction::Update { .. }) {
                    cancel_from_authorizer.cancel();
                }
                rusqlite::hooks::Authorization::Allow
            }))
            .expect("应安装取消测试 authorizer");
        let connection = Rc::new(RefCell::new(connection));
        let metrics = Arc::new(BindingMetrics::default());
        let database = build_database_table(
            &lua,
            Rc::clone(&connection),
            Arc::clone(&metrics),
            Rc::new(BindingTransactionGuard::default()),
            cancellation,
        )
        .expect("应建立数据库 Host table");

        let execute: Function = database.get("execute").expect("应取得 execute Host");
        execute
            .call::<Value>((
                "UPDATE units SET translation = 'changed' WHERE id = 'unit-1'",
                Value::Nil,
            ))
            .expect_err("SQLite 已返回 changed rows 后的取消必须终止脚本");
        assert_eq!(metrics.database_calls(), 1);
        assert_eq!(metrics.changed_rows(), 1);
        let translation: String = connection
            .borrow()
            .query_row(
                "SELECT translation FROM units WHERE id = 'unit-1'",
                [],
                |row| row.get(0),
            )
            .expect("事务内应能看到已经发生的更新");
        assert_eq!(translation, "changed");
    }

    fn deeply_nested_lua_array(lua: &Lua) -> Value {
        let mut value = Value::Integer(7);
        for _ in 0..DEEP_PROJECT_VALUE_TABLE_DEPTH {
            let table = lua.create_table().expect("应建立深层 Lua table");
            table.raw_set(1, value).expect("应写入深层 Lua table");
            value = Value::Table(table);
        }
        value
    }

    #[test]
    fn deeply_nested_table_converts_and_drops_without_using_rust_recursion() {
        let lua = Lua::new();
        let cancellation = ProjectLuaCancellation::default();
        let mut checkpoints = 0_usize;
        let mut checkpoint = || checkpoints += 1;
        let converted = lua_to_project_value_with_checkpoint(
            deeply_nested_lua_array(&lua),
            &cancellation,
            &mut checkpoint,
        )
        .expect("合法深层 table 应转换成功");

        let mut current = &converted;
        for _ in 0..DEEP_PROJECT_VALUE_TABLE_DEPTH {
            let ProjectLuaValue::Array(values) = current else {
                panic!("深层 table 的每一层都应转换成数组");
            };
            assert_eq!(values.len(), 1);
            current = &values[0];
        }
        assert!(matches!(current, ProjectLuaValue::Integer(7)));
        assert!(checkpoints > DEEP_PROJECT_VALUE_TABLE_DEPTH);
        drop(converted);
    }

    #[test]
    fn deeply_nested_partial_value_drops_safely_on_cancellation_and_error() {
        let lua = Lua::new();
        let probe_cancellation = ProjectLuaCancellation::default();
        let mut total_checkpoints = 0_usize;
        {
            let mut probe = || total_checkpoints += 1;
            drop(
                lua_to_project_value_with_checkpoint(
                    deeply_nested_lua_array(&lua),
                    &probe_cancellation,
                    &mut probe,
                )
                .expect("探测转换应成功"),
            );
        }

        let cancellation = ProjectLuaCancellation::default();
        let cancel_from_checkpoint = cancellation.clone();
        let cancel_at = total_checkpoints.saturating_mul(3) / 4;
        let mut observed = 0_usize;
        let cancelled = {
            let mut checkpoint = || {
                observed += 1;
                if observed == cancel_at {
                    cancel_from_checkpoint.cancel();
                }
            };
            match lua_to_project_value_with_checkpoint(
                deeply_nested_lua_array(&lua),
                &cancellation,
                &mut checkpoint,
            ) {
                Err(error) => error,
                Ok(_) => panic!("深层 table 转换应观察取消"),
            }
        };
        assert_eq!(cancelled.kind(), "cancelled");
        assert_eq!(observed, cancel_at);

        let invalid_root = lua.create_table().expect("应建立错误根 table");
        invalid_root
            .raw_set(true, deeply_nested_lua_array(&lua))
            .expect("应写入错误根 table");
        let invalid = match lua_to_project_value(
            Value::Table(invalid_root),
            &ProjectLuaCancellation::default(),
        ) {
            Err(error) => error,
            Ok(_) => panic!("非整数、非字符串键应被拒绝"),
        };
        assert_eq!(invalid.kind(), "invalid_table");
    }

    #[test]
    fn table_conversion_rejects_cycles_but_allows_shared_subtables() {
        let lua = Lua::new();
        let cancellation = ProjectLuaCancellation::default();

        let self_cycle = lua.create_table().expect("应建立自循环 table");
        self_cycle
            .raw_set(1, self_cycle.clone())
            .expect("应写入自循环");
        let self_error = match lua_to_project_value(Value::Table(self_cycle), &cancellation) {
            Err(error) => error,
            Ok(_) => panic!("自循环 table 必须被拒绝"),
        };
        assert_eq!(self_error.kind(), "cyclic_table");

        let first = lua.create_table().expect("应建立互循环 table");
        let second = lua.create_table().expect("应建立互循环 table");
        first.raw_set(1, second.clone()).expect("应写入互循环");
        second.raw_set(1, first.clone()).expect("应写入互循环");
        let mutual_error = match lua_to_project_value(Value::Table(first), &cancellation) {
            Err(error) => error,
            Ok(_) => panic!("互循环 table 必须被拒绝"),
        };
        assert_eq!(mutual_error.kind(), "cyclic_table");

        let child = lua.create_table().expect("应建立共享子 table");
        child.raw_set(1, 7).expect("应写入共享子 table");
        let root = lua.create_table().expect("应建立 DAG 根 table");
        root.raw_set("left", child.clone())
            .expect("应写入第一个共享引用");
        root.raw_set("right", child).expect("应写入第二个共享引用");
        let converted = lua_to_project_value(Value::Table(root), &cancellation)
            .expect("没有回边的共享子 table 应允许转换");
        let ProjectLuaValue::Object(fields) = &converted else {
            panic!("DAG 根 table 应转换为 object");
        };
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].0, "left");
        assert_eq!(fields[1].0, "right");
        for (_, value) in fields {
            let ProjectLuaValue::Array(values) = value else {
                panic!("共享子 table 的每次引用都应独立转换");
            };
            assert!(matches!(values.as_slice(), [ProjectLuaValue::Integer(7)]));
        }
    }

    #[test]
    fn long_common_prefix_object_key_sort_observes_cancellation() {
        let prefix = "x".repeat(PROJECT_LUA_SOURCE_CHUNK_BYTES.get() * 3);
        let fields = vec![
            (format!("{prefix}b"), ProjectLuaValue::Integer(2)),
            (format!("{prefix}a"), ProjectLuaValue::Integer(1)),
        ];
        let cancellation = ProjectLuaCancellation::default();
        let cancel_from_checkpoint = cancellation.clone();
        let mut observed = 0_usize;
        let result = {
            let mut checkpoint = || {
                observed += 1;
                if observed == 5 {
                    cancel_from_checkpoint.cancel();
                }
            };
            sort_project_object_fields(fields, &cancellation, &mut checkpoint)
        };

        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("长公共前缀字段排序必须观察取消"),
        };
        assert_eq!(error.kind(), "cancelled");
        assert_eq!(observed, 5);
    }

    #[test]
    fn large_script_error_value_observes_cancellation_between_chunks() {
        let lua = Lua::new();
        let bytes = vec![b'x'; PROJECT_LUA_SOURCE_CHUNK_BYTES.get() * 4];
        let value = Value::String(lua.create_string(&bytes).expect("应建立大 Lua 错误字符串"));
        let cancellation = ProjectLuaCancellation::default();
        let cancel_from_checkpoint = cancellation.clone();
        let mut observed_chunks = 0;
        let result = {
            let mut checkpoint = || {
                observed_chunks += 1;
                if observed_chunks == 2 {
                    cancel_from_checkpoint.cancel();
                }
            };
            lua_value_description_with_checkpoint(&value, &cancellation, &mut checkpoint)
        };

        assert_eq!(result, Err(ProjectLuaFailure::Cancelled));
        assert_eq!(observed_chunks, 2);
    }
}
