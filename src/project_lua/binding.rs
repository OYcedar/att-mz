use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::ffi::{CString, c_char, c_int, c_void};
use std::num::NonZeroU32;
use std::ptr;
use std::rc::Rc;
use std::sync::Arc;

use mlua::thread::ThreadStatus;
use mlua::{
    AnyUserData, Function, HookTriggers, Lua, LuaOptions, MetaMethod, MultiValue, StdLib, Table,
    UserData, UserDataFields, UserDataMethods, Value, VmState,
};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, params_from_iter};

use super::{
    PROJECT_LUA_SOURCE_CHUNK_BYTES, ProjectLuaCallError, ProjectLuaCancellation,
    ProjectLuaCompilationFailure, ProjectLuaEngineAdapter, ProjectLuaFailure, ProjectLuaPrintSink,
    ProjectLuaProgram, ProjectLuaRunRequest, ProjectLuaScriptFailure, ProjectLuaTerminologyEntry,
    ProjectLuaTranslationContext, ProjectLuaTranslationFilter, ProjectLuaTranslationRecord,
    ProjectLuaTranslationStatus,
};
use crate::diagnostic::{LuaCompilerCategory, LuaOperation};

pub(super) struct PreparedProjectLua {
    pub(super) lua: Lua,
    pub(super) function: Function,
    pub(super) connection: Rc<RefCell<Connection>>,
    pub(super) script_identity: String,
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
    install_cancellation_guards(&lua, cancellation.clone()).map_err(|_| {
        ProjectLuaFailure::Context(super::ProjectLuaContextFailure::CancellationGuard)
    })?;
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
    .map_err(|_| ProjectLuaFailure::Context(super::ProjectLuaContextFailure::InstructionHook))?;

    let function = compile_program(&lua, &request.program, &request.cancellation)?;

    let connection = Rc::new(RefCell::new(connection));
    let context = build_context(&lua, request, Rc::clone(&connection))
        .map_err(|_| ProjectLuaFailure::Context(super::ProjectLuaContextFailure::ContextTable))?;
    lua.globals()
        .set("ctx", context)
        .map_err(|_| ProjectLuaFailure::Context(super::ProjectLuaContextFailure::PublishContext))?;
    install_arguments(&lua, request)
        .map_err(|_| ProjectLuaFailure::Context(super::ProjectLuaContextFailure::Arguments))?;
    install_print(&lua, Arc::clone(&request.print_sink), cancellation)
        .map_err(|_| ProjectLuaFailure::Context(super::ProjectLuaContextFailure::PrintBinding))?;

    Ok(PreparedProjectLua {
        lua,
        function,
        connection,
        script_identity: request.program.identity().to_owned(),
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
    let lua = Lua::new_with(libraries, LuaOptions::default()).map_err(|_| {
        ProjectLuaFailure::Context(super::ProjectLuaContextFailure::RuntimeCreation)
    })?;
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
    validate_lua_source_utf8(program, cancellation)?;
    let name = CString::new(program.identity()).map_err(|_| ProjectLuaFailure::Compile {
        script_identity: program.identity().to_owned(),
        failure: ProjectLuaCompilationFailure::InvalidIdentity,
    })?;
    let mut reader = CancellableLuaSourceReader {
        source: program.source(),
        cancellation: cancellation.clone(),
        position: 0,
        cancelled: false,
    };
    let reader_data = (&mut reader as *mut CancellableLuaSourceReader).cast::<c_void>();
    let load_status = Cell::new(mlua::ffi::LUA_OK);
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
            load_status.set(status);
            if status != mlua::ffi::LUA_OK {
                mlua::ffi::lua_error(state);
            }
        })
    };
    if reader.cancelled || cancellation.is_cancelled() {
        return Err(ProjectLuaFailure::Cancelled);
    }
    result.map_err(|error| ProjectLuaFailure::Compile {
        script_identity: program.identity().to_owned(),
        failure: ProjectLuaCompilationFailure::Backend {
            category: classify_lua_compilation_error(load_status.get(), &error),
            line: lua_compilation_line(load_status.get(), &error),
        },
    })
}

fn validate_lua_source_utf8(
    program: &ProjectLuaProgram,
    cancellation: &ProjectLuaCancellation,
) -> Result<(), ProjectLuaFailure> {
    let source = program.source();
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
                return Err(ProjectLuaFailure::Compile {
                    script_identity: program.identity().to_owned(),
                    failure: ProjectLuaCompilationFailure::InvalidUtf8,
                });
            }
        }
    }
    if cancellation.is_cancelled() {
        return Err(ProjectLuaFailure::Cancelled);
    }
    if pending.is_empty() {
        Ok(())
    } else {
        Err(ProjectLuaFailure::Compile {
            script_identity: program.identity().to_owned(),
            failure: ProjectLuaCompilationFailure::InvalidUtf8,
        })
    }
}

fn classify_mlua_error(error: &mlua::Error) -> LuaCompilerCategory {
    match error {
        mlua::Error::SyntaxError { .. } => LuaCompilerCategory::Syntax,
        mlua::Error::MemoryError(_) => LuaCompilerCategory::Memory,
        mlua::Error::SafetyError(_) => LuaCompilerCategory::Safety,
        mlua::Error::CallbackError { .. } => LuaCompilerCategory::Callback,
        mlua::Error::ExternalError(_) => LuaCompilerCategory::External,
        _ => LuaCompilerCategory::Unknown,
    }
}

/// 编译类别只来自 `lua_load` 的类型化状态码，不从供应商正文猜测。
fn classify_lua_compilation_error(load_status: c_int, error: &mlua::Error) -> LuaCompilerCategory {
    match load_status {
        mlua::ffi::LUA_ERRSYNTAX => LuaCompilerCategory::Syntax,
        mlua::ffi::LUA_ERRMEM => LuaCompilerCategory::Memory,
        _ => classify_mlua_error(error),
    }
}

fn lua_compilation_line(load_status: c_int, error: &mlua::Error) -> Option<usize> {
    if load_status != mlua::ffi::LUA_ERRSYNTAX {
        return None;
    }
    let message = match error {
        mlua::Error::SyntaxError { message, .. } | mlua::Error::RuntimeError(message) => message,
        _ => return None,
    };
    lua_syntax_message_line(message)
}

/// Lua 5.4 只在类型化 SyntaxError 的后端 message 字段中提供源码行号。
/// 已由 `lua_load` 状态确认语法错误后，这里只提取首个 `:<decimal>:` 坐标，
/// 不使用正文判断类别，也不保留或转发编译器正文。
fn lua_syntax_message_line(message: &str) -> Option<usize> {
    let bytes = message.as_bytes();
    for colon in bytes
        .iter()
        .enumerate()
        .filter_map(|(index, byte)| (*byte == b':').then_some(index))
    {
        let start = colon + 1;
        let digits = bytes[start..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if digits == 0 || bytes.get(start + digits) != Some(&b':') {
            continue;
        }
        if let Ok(line) = message[start..start + digits].parse::<usize>()
            && line > 0
        {
            return Some(line);
        }
    }
    None
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
        globals.raw_set(name, Value::Nil).map_err(|_| {
            ProjectLuaFailure::Context(super::ProjectLuaContextFailure::RemoveExternalCapability)
        })?;
    }
    Ok(())
}

fn build_context(
    lua: &Lua,
    request: &ProjectLuaRunRequest,
    connection: Rc<RefCell<Connection>>,
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
        build_database_table(lua, Rc::clone(&connection), request.cancellation.clone())?,
    )?;
    context.set(
        "translation",
        build_translation_table(
            lua,
            Rc::clone(&connection),
            Arc::clone(&request.adapter),
            request.cancellation.clone(),
        )?,
    )?;
    context.set(
        "terminology",
        build_terminology_table(
            lua,
            connection,
            Arc::clone(&request.adapter),
            request.cancellation.clone(),
        )?,
    )?;
    Ok(context)
}

fn install_print(
    lua: &Lua,
    sink: Arc<dyn ProjectLuaPrintSink>,
    cancellation: ProjectLuaCancellation,
) -> mlua::Result<()> {
    let native = lua.create_function(move |lua, bytes: mlua::LuaString| {
        ensure_lua_not_cancelled(&cancellation)?;
        let result = sink
            .print(bytes.as_bytes().as_ref())
            .map_err(|error| host_error("log", error, LuaOperation::ExecuteScript));
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
    cancellation: ProjectLuaCancellation,
) -> mlua::Result<Table> {
    let native = lua.create_table()?;
    let null = lua.create_userdata(LuaSqliteNull)?;

    let query_connection = Rc::clone(&connection);
    let query_cancellation = cancellation.clone();
    native.set(
        "query",
        lua.create_function(move |lua, (statement, parameters): (Value, Value)| {
            ensure_lua_not_cancelled(&query_cancellation)?;
            let connection = query_connection.borrow();
            let result = parse_sql_call(statement, parameters, &query_cancellation)
                .map_err(|error| host_error("binding", error, LuaOperation::QueryDatabase))
                .and_then(|(statement, parameters)| {
                    query_database(&connection, &statement, &parameters, &query_cancellation)
                        .map_err(|error| match error {
                            DatabaseQueryError::Sqlite(error) => {
                                sqlite_host_error(LuaOperation::QueryDatabase, error)
                            }
                            DatabaseQueryError::Binding(error) => {
                                host_error("binding", error, LuaOperation::QueryDatabase)
                            }
                        })
                });
            let output = host_result_to_lua(lua, result, |lua, rows| {
                rows_to_lua(lua, rows, &query_cancellation)
            });
            ensure_lua_not_cancelled(&query_cancellation)?;
            output
        })?,
    )?;

    let execute_cancellation = cancellation.clone();
    native.set(
        "execute",
        lua.create_function(move |lua, (statement, parameters): (Value, Value)| {
            ensure_lua_not_cancelled(&execute_cancellation)?;
            let connection = connection.borrow();
            let result = parse_sql_call(statement, parameters, &execute_cancellation)
                .map_err(|error| host_error("binding", error, LuaOperation::QueryDatabase))
                .and_then(|(statement, parameters)| {
                    ensure_project_lua_call_running(&execute_cancellation).map_err(|error| {
                        host_error("binding", error, LuaOperation::QueryDatabase)
                    })?;
                    execute_database(&connection, &statement, &parameters)
                        .map_err(|error| sqlite_host_error(LuaOperation::QueryDatabase, error))
                });
            let output = match result {
                Ok(changed) => host_result_to_lua(lua, Ok(changed), |_lua, changed| {
                    i64::try_from(changed)
                        .map(Value::Integer)
                        .map_err(|_| mlua::Error::runtime("SQLite 受影响行数超出 Lua integer"))
                }),
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
                    .map_err(|error| host_error("binding", error, LuaOperation::QueryDatabase))
            }
            _ => Err(LuaHostCallError::binding(LuaOperation::QueryDatabase)),
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
    cancellation: ProjectLuaCancellation,
) -> mlua::Result<Table> {
    let native = lua.create_table()?;

    let list_adapter = Arc::clone(&adapter);
    let list_connection = Rc::clone(&connection);
    let list_cancellation = cancellation.clone();
    native.set(
        "list",
        lua.create_function(move |lua, filter: Value| {
            ensure_lua_not_cancelled(&list_cancellation)?;
            let connection = list_connection.borrow();
            let result = parse_translation_filter(filter, &list_cancellation)
                .map_err(|error| host_error("binding", error, LuaOperation::QueryDatabase))
                .and_then(|filter| {
                    list_adapter
                        .list_translations(&connection, filter)
                        .map_err(|error| {
                            host_error("translation", error, LuaOperation::QueryDatabase)
                        })
                });
            let output = host_result_to_lua(lua, result, |lua, records| {
                translation_records_to_lua(lua, records, &list_cancellation)
            });
            ensure_lua_not_cancelled(&list_cancellation)?;
            output
        })?,
    )?;

    let context_adapter = Arc::clone(&adapter);
    let context_connection = Rc::clone(&connection);
    let context_cancellation = cancellation.clone();
    native.set(
        "context",
        lua.create_function(move |lua, ids: Value| {
            ensure_lua_not_cancelled(&context_cancellation)?;
            let connection = context_connection.borrow();
            let result = parse_text_array(ids, "ids", &context_cancellation)
                .map_err(|error| host_error("binding", error, LuaOperation::QueryDatabase))
                .and_then(|ids| {
                    context_adapter
                        .translation_context(&connection, ids)
                        .map_err(|error| {
                            host_error("translation", error, LuaOperation::QueryDatabase)
                        })
                });
            let output = host_result_to_lua(lua, result, |lua, contexts| {
                translation_contexts_to_lua(lua, contexts, &context_cancellation)
            });
            ensure_lua_not_cancelled(&context_cancellation)?;
            output
        })?,
    )?;

    let set_adapter = Arc::clone(&adapter);
    let set_connection = Rc::clone(&connection);
    let set_cancellation = cancellation.clone();
    native.set(
        "set",
        lua.create_function(move |lua, (id, translation): (Value, Value)| {
            ensure_lua_not_cancelled(&set_cancellation)?;
            let connection = set_connection.borrow();
            let result = parse_text(id, "id", &set_cancellation)
                .and_then(|id| {
                    parse_text_array(translation, "translation", &set_cancellation)
                        .map(|translation| (id, translation))
                })
                .map_err(|error| host_error("binding", error, LuaOperation::SetTranslation))
                .and_then(|(id, translation)| {
                    set_adapter
                        .set_translation(&connection, id, translation)
                        .map_err(|error| {
                            host_error("translation", error, LuaOperation::SetTranslation)
                        })
                });
            let output = match result {
                Ok(_changed) => host_result_to_lua(lua, Ok(()), |_lua, ()| Ok(Value::Nil)),
                Err(error) => host_result_to_lua(lua, Err(error), |_lua, ()| Ok(Value::Nil)),
            };
            ensure_lua_not_cancelled(&set_cancellation)?;
            output
        })?,
    )?;

    let clear_cancellation = cancellation;
    native.set(
        "clear",
        lua.create_function(move |lua, id: Value| {
            ensure_lua_not_cancelled(&clear_cancellation)?;
            let connection = connection.borrow();
            let result = parse_text(id, "id", &clear_cancellation)
                .map_err(|error| host_error("binding", error, LuaOperation::ClearTranslation))
                .and_then(|id| {
                    adapter.clear_translation(&connection, id).map_err(|error| {
                        host_error("translation", error, LuaOperation::ClearTranslation)
                    })
                });
            let output = match result {
                Ok(_changed) => host_result_to_lua(lua, Ok(()), |_lua, ()| Ok(Value::Nil)),
                Err(error) => host_result_to_lua(lua, Err(error), |_lua, ()| Ok(Value::Nil)),
            };
            ensure_lua_not_cancelled(&clear_cancellation)?;
            output
        })?,
    )?;

    checked_function_table(lua, native, &["list", "context", "set", "clear"])
}

fn build_terminology_table(
    lua: &Lua,
    connection: Rc<RefCell<Connection>>,
    adapter: Arc<dyn ProjectLuaEngineAdapter>,
    cancellation: ProjectLuaCancellation,
) -> mlua::Result<Table> {
    let native = lua.create_table()?;
    native.set(
        "list",
        lua.create_function(move |lua, (): ()| {
            ensure_lua_not_cancelled(&cancellation)?;
            let connection = connection.borrow();
            let result = adapter
                .list_terminology(&connection)
                .map_err(|error| host_error("terminology", error, LuaOperation::QueryDatabase));
            let output = host_result_to_lua(lua, result, |lua, entries| {
                terminology_entries_to_lua(lua, entries, &cancellation)
            });
            ensure_lua_not_cancelled(&cancellation)?;
            output
        })?,
    )?;
    checked_function_table(lua, native, &["list"])
}

fn parse_translation_filter(
    value: Value,
    cancellation: &ProjectLuaCancellation,
) -> Result<ProjectLuaTranslationFilter, ProjectLuaCallError> {
    ensure_project_lua_call_running(cancellation)?;
    let Value::Table(table) = value else {
        return match value {
            Value::Nil => Ok(ProjectLuaTranslationFilter::default()),
            _ => Err(ProjectLuaCallError::violation(
                crate::diagnostic::LuaValueViolation::UnexpectedType,
            )
            .with_field("filter")),
        };
    };
    let mut status = None;
    let mut ids = None;
    for pair in table.pairs::<Value, Value>() {
        ensure_project_lua_call_running(cancellation)?;
        let (key, value) = pair.map_err(|_| {
            ProjectLuaCallError::violation(crate::diagnostic::LuaValueViolation::InvalidTable)
                .with_field("filter")
        })?;
        let Value::String(key) = key else {
            return Err(ProjectLuaCallError::violation(
                crate::diagnostic::LuaValueViolation::InvalidTable,
            )
            .with_field("filter"));
        };
        let key = strict_text(&key, "filter 字段", cancellation)?;
        match key.as_str() {
            "status" if status.is_none() => {
                let value = parse_text(value, "status", cancellation)?;
                status = Some(ProjectLuaTranslationStatus::parse(&value).ok_or_else(|| {
                    ProjectLuaCallError::violation(
                        crate::diagnostic::LuaValueViolation::UnexpectedType,
                    )
                    .with_field("status")
                })?);
            }
            "ids" if ids.is_none() => {
                ids = Some(parse_text_array(value, "ids", cancellation)?);
            }
            _ => {
                return Err(ProjectLuaCallError::violation(
                    crate::diagnostic::LuaValueViolation::InvalidTable,
                )
                .with_field("filter"));
            }
        }
    }
    Ok(ProjectLuaTranslationFilter { status, ids })
}

fn parse_text(
    value: Value,
    field: &'static str,
    cancellation: &ProjectLuaCancellation,
) -> Result<String, ProjectLuaCallError> {
    let Value::String(value) = value else {
        return Err(ProjectLuaCallError::violation(
            crate::diagnostic::LuaValueViolation::UnexpectedType,
        )
        .with_field(field));
    };
    strict_text(&value, field, cancellation).map_err(|error| error.with_field(field))
}

fn parse_text_array(
    value: Value,
    field: &'static str,
    cancellation: &ProjectLuaCancellation,
) -> Result<Vec<String>, ProjectLuaCallError> {
    let Value::Table(table) = value else {
        return Err(ProjectLuaCallError::violation(
            crate::diagnostic::LuaValueViolation::UnexpectedType,
        )
        .with_field(field));
    };
    let values =
        dense_table_values(table, cancellation).map_err(|error| error.with_field(field))?;
    let mut result = Vec::with_capacity(values.len());
    for value in values {
        ensure_project_lua_call_running(cancellation)?;
        result.push(parse_text(value, field, cancellation)?);
    }
    Ok(result)
}

fn translation_records_to_lua(
    lua: &Lua,
    records: Vec<ProjectLuaTranslationRecord>,
    cancellation: &ProjectLuaCancellation,
) -> mlua::Result<Value> {
    let result = lua.create_table_with_capacity(records.len(), 0)?;
    for (index, record) in records.into_iter().enumerate() {
        ensure_lua_not_cancelled(cancellation)?;
        result.raw_set(
            index + 1,
            translation_record_to_lua(lua, record, cancellation)?,
        )?;
    }
    Ok(Value::Table(result))
}

fn translation_record_to_lua(
    lua: &Lua,
    record: ProjectLuaTranslationRecord,
    cancellation: &ProjectLuaCancellation,
) -> mlua::Result<Table> {
    ensure_lua_not_cancelled(cancellation)?;
    let table = lua.create_table()?;
    table.set("id", record.id)?;
    table.set("type", record.kind)?;
    table.set(
        "source",
        text_array_to_lua(lua, record.source, cancellation)?,
    )?;
    if let Some(translation) = record.translation {
        table.set(
            "translation",
            text_array_to_lua(lua, translation, cancellation)?,
        )?;
    }
    table.set("status", record.status.as_str())?;
    if let Some(origin) = record.origin {
        table.set("origin", origin)?;
    }
    if let Some(outdated) = record.outdated_manual {
        let snapshot = lua.create_table()?;
        snapshot.set("id", outdated.id)?;
        snapshot.set("type", outdated.kind)?;
        snapshot.set(
            "source",
            text_array_to_lua(lua, outdated.source, cancellation)?,
        )?;
        snapshot.set(
            "translation",
            text_array_to_lua(lua, outdated.translation, cancellation)?,
        )?;
        table.set("outdated_manual", snapshot)?;
    }
    Ok(table)
}

fn translation_contexts_to_lua(
    lua: &Lua,
    contexts: Vec<ProjectLuaTranslationContext>,
    cancellation: &ProjectLuaCancellation,
) -> mlua::Result<Value> {
    let result = lua.create_table_with_capacity(contexts.len(), 0)?;
    for (index, context) in contexts.into_iter().enumerate() {
        ensure_lua_not_cancelled(cancellation)?;
        let table = lua.create_table()?;
        table.set("id", context.id)?;
        if let Some(speaker) = context.speaker {
            table.set("speaker", speaker)?;
        }
        table.set(
            "translations",
            translation_records_to_lua(lua, context.translations, cancellation)?,
        )?;
        result.raw_set(index + 1, table)?;
    }
    Ok(Value::Table(result))
}

fn terminology_entries_to_lua(
    lua: &Lua,
    entries: Vec<ProjectLuaTerminologyEntry>,
    cancellation: &ProjectLuaCancellation,
) -> mlua::Result<Value> {
    let result = lua.create_table_with_capacity(entries.len(), 0)?;
    for (index, entry) in entries.into_iter().enumerate() {
        ensure_lua_not_cancelled(cancellation)?;
        let table = lua.create_table()?;
        table.set("term", entry.term)?;
        table.set("translation", entry.translation)?;
        result.raw_set(index + 1, table)?;
    }
    Ok(Value::Table(result))
}

fn text_array_to_lua(
    lua: &Lua,
    values: Vec<String>,
    cancellation: &ProjectLuaCancellation,
) -> mlua::Result<Table> {
    let result = lua.create_table_with_capacity(values.len(), 0)?;
    for (index, value) in values.into_iter().enumerate() {
        ensure_lua_not_cancelled(cancellation)?;
        result.raw_set(index + 1, value)?;
    }
    Ok(result)
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
        _ => {
            return Err(ProjectLuaCallError::violation(
                crate::diagnostic::LuaValueViolation::UnexpectedType,
            )
            .with_field("statement"));
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
        _ => {
            return Err(ProjectLuaCallError::violation(
                crate::diagnostic::LuaValueViolation::UnexpectedType,
            )
            .with_field("parameters"));
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
        Value::Number(_) => Err(ProjectLuaCallError::violation(
            crate::diagnostic::LuaValueViolation::UnexpectedType,
        )),
        Value::String(value) => {
            strict_text(&value, "SQLite TEXT", cancellation).map(rusqlite::types::Value::Text)
        }
        Value::UserData(value) if value.is::<LuaSqliteNull>() => Ok(rusqlite::types::Value::Null),
        Value::UserData(value) if value.is::<LuaBlob>() => {
            let value = value.borrow::<LuaBlob>().map_err(|_| {
                ProjectLuaCallError::violation(crate::diagnostic::LuaValueViolation::InvalidBlob)
            })?;
            clone_bytes_with_cancellation(&value.bytes, cancellation)
                .map(rusqlite::types::Value::Blob)
        }
        _ => Err(ProjectLuaCallError::violation(
            crate::diagnostic::LuaValueViolation::UnexpectedType,
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
    if statement.column_count() == 0 {
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
        let (key, value) = pair.map_err(|_| {
            ProjectLuaCallError::violation(crate::diagnostic::LuaValueViolation::InvalidArrayIndex)
        })?;
        let Value::Integer(index) = key else {
            return Err(ProjectLuaCallError::violation(
                crate::diagnostic::LuaValueViolation::InvalidArrayIndex,
            ));
        };
        let index = usize::try_from(index)
            .ok()
            .filter(|index| *index > 0)
            .ok_or_else(|| {
                ProjectLuaCallError::violation(
                    crate::diagnostic::LuaValueViolation::InvalidArrayIndex,
                )
            })?;
        if indexed.insert(index, value).is_some() {
            return Err(ProjectLuaCallError::violation(
                crate::diagnostic::LuaValueViolation::SparseArray,
            ));
        }
    }
    let mut values = Vec::with_capacity(indexed.len());
    for (offset, (index, value)) in indexed.into_iter().enumerate() {
        ensure_project_lua_call_running(cancellation)?;
        if index != offset + 1 {
            return Err(ProjectLuaCallError::violation(
                crate::diagnostic::LuaValueViolation::SparseArray,
            ));
        }
        values.push(value);
    }
    ensure_project_lua_call_running(cancellation)?;
    Ok(values)
}

fn ensure_project_lua_call_running(
    cancellation: &ProjectLuaCancellation,
) -> Result<(), ProjectLuaCallError> {
    if cancellation.is_cancelled() {
        Err(ProjectLuaCallError::cancelled())
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
    _role: &str,
    cancellation: &ProjectLuaCancellation,
) -> Result<String, ProjectLuaCallError> {
    match clone_utf8_text_with_cancellation(value.as_bytes().as_ref(), cancellation)? {
        Ok(value) => Ok(value),
        Err(_) => Err(ProjectLuaCallError::violation(
            crate::diagnostic::LuaValueViolation::InvalidUtf8,
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
    operation: LuaOperation,
    error: ProjectLuaCallError,
}

impl LuaHostCallError {
    fn binding(operation: LuaOperation) -> Self {
        Self {
            domain: "binding",
            operation,
            error: ProjectLuaCallError::violation(
                crate::diagnostic::LuaValueViolation::UnexpectedType,
            )
            .with_operation(operation),
        }
    }
}

impl UserData for LuaHostCallError {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("domain", |_lua, this| Ok(this.domain));
        fields.add_field_method_get("kind", |_lua, this| Ok(this.error.kind()));
        fields.add_field_method_get("operation", |_lua, this| {
            Ok(super::lua_host_operation_name(this.operation))
        });
        fields.add_field_method_get("message", |_lua, this| Ok(this.error.message().to_owned()));
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::ToString, |_lua, this, ()| {
            Ok(this.error.message().to_owned())
        });
    }
}

fn host_error(
    domain: &'static str,
    error: ProjectLuaCallError,
    operation: LuaOperation,
) -> LuaHostCallError {
    LuaHostCallError {
        domain,
        operation,
        error: error.with_operation(operation),
    }
}

fn sqlite_host_error(operation: LuaOperation, error: rusqlite::Error) -> LuaHostCallError {
    LuaHostCallError {
        domain: "sqlite",
        operation,
        error: ProjectLuaCallError::sqlite(error).with_operation(operation),
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
            Err(_error) => Ok(MultiValue::from_vec(vec![
                Value::Boolean(false),
                Value::UserData(
                    lua.create_userdata(LuaHostCallError::binding(LuaOperation::ExecuteScript))?,
                ),
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
    script_identity: &str,
    engine: crate::diagnostic::LuaEngine,
    cancellation: &ProjectLuaCancellation,
) -> Result<(), ProjectLuaFailure> {
    let runner: Function = lua
        .load(
            "return function(main) local ok, value = xpcall(main, function(error) return error end); return ok, value end",
        )
        .eval()
        .map_err(|_| ProjectLuaFailure::Context(super::ProjectLuaContextFailure::ProtectedCallWrapper))?;
    let thread = lua
        .create_thread(runner)
        .map_err(|_| ProjectLuaFailure::Context(super::ProjectLuaContextFailure::ThreadCreation))?;
    let result = thread.resume::<(bool, Value)>(function);
    if cancellation.is_cancelled() {
        return Err(ProjectLuaFailure::Cancelled);
    }
    if thread.status() == ThreadStatus::Resumable {
        return Err(ProjectLuaFailure::Script {
            script_identity: script_identity.to_owned(),
            failure: ProjectLuaScriptFailure::Yielded,
        });
    }
    let (succeeded, error) = result.map_err(|error| ProjectLuaFailure::Script {
        script_identity: script_identity.to_owned(),
        failure: ProjectLuaScriptFailure::Backend(classify_mlua_error(&error)),
    })?;
    if succeeded {
        return Ok(());
    }
    if let Value::UserData(userdata) = &error
        && let Ok(error) = userdata.borrow::<LuaHostCallError>()
    {
        if cancellation.is_cancelled() {
            return Err(ProjectLuaFailure::Cancelled);
        }
        return Err(ProjectLuaFailure::Host(
            error.error.clone().with_engine(engine),
        ));
    }
    drop(error);
    Err(ProjectLuaFailure::Script {
        script_identity: script_identity.to_owned(),
        failure: ProjectLuaScriptFailure::NonErrorValue,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_host_entry_returns_an_error() {
        let lua = Lua::new();
        let cancellation = ProjectLuaCancellation::default();
        let database = build_database_table(
            &lua,
            Rc::new(RefCell::new(
                Connection::open_in_memory().expect("应建立测试数据库"),
            )),
            cancellation.clone(),
        )
        .expect("应建立数据库 Host table");
        cancellation.cancel();

        let query: Function = database.get("query").expect("应取得 query Host");
        query
            .call::<Value>(("SELECT 1", Value::Nil))
            .expect_err("进入 Host 后观察到取消必须返回失败");
    }

    #[test]
    fn completed_update_is_visible_before_post_execute_cancellation() {
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
        let database = build_database_table(&lua, Rc::clone(&connection), cancellation)
            .expect("应建立数据库 Host table");

        let execute: Function = database.get("execute").expect("应取得 execute Host");
        execute
            .call::<Value>((
                "UPDATE units SET translation = 'changed' WHERE id = 'unit-1'",
                Value::Nil,
            ))
            .expect_err("SQLite 已返回 changed rows 后的取消必须终止脚本");
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
}
