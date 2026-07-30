use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashSet};
use std::ffi::c_void;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::Arc;

use mlua::{
    AnyUserData, Function, HookTriggers, Lua, LuaOptions, MetaMethod, MultiValue, StdLib, Table,
    UserData, UserDataFields, UserDataMethods, Value, VmState,
};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, params_from_iter};

use super::{
    ProjectLuaCallError, ProjectLuaEngineAdapter, ProjectLuaFailure, ProjectLuaPrintSink,
    ProjectLuaProgram, ProjectLuaRunRequest, ProjectLuaValue,
};

pub(super) struct PreparedProjectLua {
    pub(super) lua: Lua,
    pub(super) function: Function,
    pub(super) connection: Rc<RefCell<Connection>>,
    pub(super) metrics: Rc<BindingMetrics>,
    pub(super) transaction_guard: Rc<BindingTransactionGuard>,
}

#[derive(Debug, Default)]
pub(super) struct BindingMetrics {
    database_calls: Cell<u64>,
    changed_rows: Cell<u64>,
    translation_calls: Cell<u64>,
    printed_lines: Cell<u64>,
}

impl BindingMetrics {
    pub(super) fn database_calls(&self) -> u64 {
        self.database_calls.get()
    }

    pub(super) fn changed_rows(&self) -> u64 {
        self.changed_rows.get()
    }

    pub(super) fn translation_calls(&self) -> u64 {
        self.translation_calls.get()
    }

    pub(super) fn printed_lines(&self) -> u64 {
        self.printed_lines.get()
    }

    fn record_database_call(&self) {
        self.database_calls
            .set(self.database_calls.get().saturating_add(1));
    }

    fn record_changed_rows(&self, rows: u64) {
        self.changed_rows
            .set(self.changed_rows.get().saturating_add(rows));
    }

    fn record_translation_call(&self) {
        self.translation_calls
            .set(self.translation_calls.get().saturating_add(1));
    }

    fn record_printed_line(&self) {
        self.printed_lines
            .set(self.printed_lines.get().saturating_add(1));
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

pub(super) fn prepare_lua(
    connection: Connection,
    request: &ProjectLuaRunRequest,
    cancel_check_instruction_interval: NonZeroU32,
) -> Result<PreparedProjectLua, ProjectLuaFailure> {
    let lua = new_restricted_lua()?;

    let cancellation = request.cancellation.clone();
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(cancel_check_instruction_interval.get()),
        move |_lua, _debug| {
            if cancellation.is_cancelled() {
                Err(mlua::Error::runtime("ATT_PROJECT_LUA_CANCELLED"))
            } else {
                Ok(VmState::Continue)
            }
        },
    )
    .map_err(|error| ProjectLuaFailure::Context(error.to_string()))?;

    let function = compile_program(&lua, &request.program)?;

    let connection = Rc::new(RefCell::new(connection));
    let metrics = Rc::new(BindingMetrics::default());
    let transaction_guard = Rc::new(BindingTransactionGuard::default());
    let context = build_context(
        &lua,
        request,
        Rc::clone(&connection),
        Rc::clone(&metrics),
        Rc::clone(&transaction_guard),
    )
    .map_err(|error| ProjectLuaFailure::Context(error.to_string()))?;
    lua.globals()
        .set("ctx", context)
        .map_err(|error| ProjectLuaFailure::Context(error.to_string()))?;
    install_arguments(&lua, request)
        .map_err(|error| ProjectLuaFailure::Context(error.to_string()))?;
    install_print(&lua, Arc::clone(&request.print_sink), Rc::clone(&metrics))
        .map_err(|error| ProjectLuaFailure::Context(error.to_string()))?;

    Ok(PreparedProjectLua {
        lua,
        function,
        connection,
        metrics,
        transaction_guard,
    })
}

pub(super) fn validate_program(program: &ProjectLuaProgram) -> Result<(), ProjectLuaFailure> {
    let lua = new_restricted_lua()?;
    let _function = compile_program(&lua, program)?;
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

fn compile_program(lua: &Lua, program: &ProjectLuaProgram) -> Result<Function, ProjectLuaFailure> {
    let source = std::str::from_utf8(program.source())
        .map_err(|_| ProjectLuaFailure::Compile("Lua 主程序必须是有效 UTF-8".to_owned()))?;
    lua.load(source)
        .set_name(program.identity())
        .into_function()
        .map_err(|error| ProjectLuaFailure::Compile(error.to_string()))
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
    metrics: Rc<BindingMetrics>,
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
            Rc::clone(&metrics),
            Rc::clone(&transaction_guard),
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
        )?,
    )?;
    Ok(context)
}

fn install_print(
    lua: &Lua,
    sink: Arc<dyn ProjectLuaPrintSink>,
    metrics: Rc<BindingMetrics>,
) -> mlua::Result<()> {
    let native = lua.create_function(move |lua, bytes: mlua::LuaString| {
        metrics.record_printed_line();
        let result = sink
            .print(bytes.as_bytes().as_ref())
            .map_err(|error| host_error("log", error, "print"));
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
    metrics: Rc<BindingMetrics>,
    transaction_guard: Rc<BindingTransactionGuard>,
) -> mlua::Result<Table> {
    let native = lua.create_table()?;
    let null = lua.create_userdata(LuaSqliteNull)?;

    let query_connection = Rc::clone(&connection);
    let query_metrics = Rc::clone(&metrics);
    let query_transaction_guard = Rc::clone(&transaction_guard);
    native.set(
        "query",
        lua.create_function(move |lua, (statement, parameters): (Value, Value)| {
            query_metrics.record_database_call();
            let connection = query_connection.borrow();
            let result = query_transaction_guard.call(&connection, "db.query", |connection| {
                parse_sql_call(statement, parameters)
                    .map_err(|error| host_error("binding", error, "db.query"))
                    .and_then(|(statement, parameters)| {
                        query_database(connection, &statement, &parameters)
                            .map_err(|error| sqlite_host_error("db.query", &error))
                    })
            });
            host_result_to_lua(lua, result, rows_to_lua)
        })?,
    )?;

    let execute_metrics = Rc::clone(&metrics);
    let execute_transaction_guard = transaction_guard;
    native.set(
        "execute",
        lua.create_function(move |lua, (statement, parameters): (Value, Value)| {
            execute_metrics.record_database_call();
            let connection = connection.borrow();
            let result = execute_transaction_guard.call(&connection, "db.execute", |connection| {
                parse_sql_call(statement, parameters)
                    .map_err(|error| host_error("binding", error, "db.execute"))
                    .and_then(|(statement, parameters)| {
                        execute_database(connection, &statement, &parameters)
                            .map_err(|error| sqlite_host_error("db.execute", &error))
                    })
            });
            match result {
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
            }
        })?,
    )?;

    let blob = lua.create_function(move |lua, value: Value| {
        let result = match value {
            Value::String(bytes) => Ok(bytes.as_bytes().to_vec()),
            other => Err(LuaHostCallError::binding(
                "db.blob",
                format!(
                    "ctx.db.blob 的参数必须是字符串，实际为 {}",
                    other.type_name()
                ),
            )),
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
    metrics: Rc<BindingMetrics>,
    transaction_guard: Rc<BindingTransactionGuard>,
) -> mlua::Result<Table> {
    let native = lua.create_table()?;

    let set_adapter = Arc::clone(&adapter);
    let set_connection = Rc::clone(&connection);
    let set_metrics = Rc::clone(&metrics);
    let set_transaction_guard = Rc::clone(&transaction_guard);
    native.set(
        "set",
        lua.create_function(move |lua, (locator, translation): (Value, Value)| {
            let connection = set_connection.borrow();
            let result = set_transaction_guard.call(&connection, "translation.set", |connection| {
                lua_to_project_value(locator)
                    .and_then(|locator| {
                        lua_to_project_value(translation).map(|translation| (locator, translation))
                    })
                    .map_err(|error| host_error("binding", error, "translation.set"))
                    .and_then(|(locator, translation)| {
                        set_adapter
                            .set_translation(connection, locator, translation)
                            .map_err(|error| host_error("translation", error, "translation.set"))
                    })
            });
            match result {
                Ok(changed) => {
                    set_metrics.record_translation_call();
                    set_metrics.record_changed_rows(changed);
                    host_result_to_lua(lua, Ok(()), |_lua, ()| Ok(Value::Nil))
                }
                Err(error) => host_result_to_lua(lua, Err(error), |_lua, ()| Ok(Value::Nil)),
            }
        })?,
    )?;

    let clear_metrics = metrics;
    let clear_transaction_guard = transaction_guard;
    native.set(
        "clear",
        lua.create_function(move |lua, locator: Value| {
            let connection = connection.borrow();
            let result =
                clear_transaction_guard.call(&connection, "translation.clear", |connection| {
                    lua_to_project_value(locator)
                        .map_err(|error| host_error("binding", error, "translation.clear"))
                        .and_then(|locator| {
                            adapter
                                .clear_translation(connection, locator)
                                .map_err(|error| {
                                    host_error("translation", error, "translation.clear")
                                })
                        })
                });
            match result {
                Ok(changed) => {
                    clear_metrics.record_translation_call();
                    clear_metrics.record_changed_rows(changed);
                    host_result_to_lua(lua, Ok(()), |_lua, ()| Ok(Value::Nil))
                }
                Err(error) => host_result_to_lua(lua, Err(error), |_lua, ()| Ok(Value::Nil)),
            }
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
) -> Result<(String, Vec<rusqlite::types::Value>), ProjectLuaCallError> {
    let statement = match statement {
        Value::String(statement) => strict_text(&statement, "SQL")?,
        value => {
            return Err(ProjectLuaCallError::new(
                "invalid_statement",
                format!("SQL 必须是字符串，实际为 {}", value.type_name()),
            ));
        }
    };
    let parameters = match parameters {
        Value::Nil => Vec::new(),
        Value::Table(table) => dense_table_values(table)?
            .into_iter()
            .map(lua_to_sqlite_value)
            .collect::<Result<Vec<_>, _>>()?,
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
    Ok((statement, parameters))
}

fn lua_to_sqlite_value(value: Value) -> Result<rusqlite::types::Value, ProjectLuaCallError> {
    match value {
        Value::Integer(value) => Ok(rusqlite::types::Value::Integer(value)),
        Value::Number(value) if value.is_finite() => Ok(rusqlite::types::Value::Real(value)),
        Value::Number(_) => Err(ProjectLuaCallError::new(
            "invalid_real",
            "SQLite REAL 参数不得为 NaN 或 Inf",
        )),
        Value::String(value) => {
            strict_text(&value, "SQLite TEXT").map(rusqlite::types::Value::Text)
        }
        Value::UserData(value) if value.is::<LuaSqliteNull>() => Ok(rusqlite::types::Value::Null),
        Value::UserData(value) if value.is::<LuaBlob>() => Ok(rusqlite::types::Value::Blob(
            value
                .borrow::<LuaBlob>()
                .map_err(|error| ProjectLuaCallError::new("invalid_blob", error.to_string()))?
                .0
                .clone(),
        )),
        other => Err(ProjectLuaCallError::new(
            "unsupported_parameter",
            format!("SQLite 参数不支持 {}", other.type_name()),
        )),
    }
}

fn query_database(
    connection: &Connection,
    sql: &str,
    parameters: &[rusqlite::types::Value],
) -> rusqlite::Result<Vec<Vec<rusqlite::types::Value>>> {
    let mut statement = connection.prepare(sql)?;
    if !statement.readonly() || statement.column_count() == 0 {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let column_count = statement.column_count();
    let mut rows = statement.query(params_from_iter(parameters.iter()))?;
    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        let mut values = Vec::with_capacity(column_count);
        for index in 0..column_count {
            values.push(owned_sqlite_value(row.get_ref(index)?)?);
        }
        result.push(values);
    }
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

fn owned_sqlite_value(value: ValueRef<'_>) -> rusqlite::Result<rusqlite::types::Value> {
    match value {
        ValueRef::Null => Ok(rusqlite::types::Value::Null),
        ValueRef::Integer(value) => Ok(rusqlite::types::Value::Integer(value)),
        ValueRef::Real(value) => Ok(rusqlite::types::Value::Real(value)),
        ValueRef::Text(value) => std::str::from_utf8(value)
            .map(|value| rusqlite::types::Value::Text(value.to_owned()))
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            }),
        ValueRef::Blob(value) => Ok(rusqlite::types::Value::Blob(value.to_vec())),
    }
}

fn rows_to_lua(lua: &Lua, rows: Vec<Vec<rusqlite::types::Value>>) -> mlua::Result<Value> {
    let result = lua.create_table_with_capacity(rows.len(), 0)?;
    for (row_index, row) in rows.into_iter().enumerate() {
        let values = lua.create_table_with_capacity(row.len(), 0)?;
        for (column_index, value) in row.into_iter().enumerate() {
            values.raw_set(column_index + 1, sqlite_to_lua_value(lua, value)?)?;
        }
        result.raw_set(row_index + 1, values)?;
    }
    Ok(Value::Table(result))
}

fn sqlite_to_lua_value(lua: &Lua, value: rusqlite::types::Value) -> mlua::Result<Value> {
    match value {
        rusqlite::types::Value::Null => lua.create_userdata(LuaSqliteNull).map(Value::UserData),
        rusqlite::types::Value::Integer(value) => Ok(Value::Integer(value)),
        rusqlite::types::Value::Real(value) if value.is_finite() => Ok(Value::Number(value)),
        rusqlite::types::Value::Real(_) => {
            Err(mlua::Error::runtime("SQLite REAL 结果为 NaN 或 Inf"))
        }
        rusqlite::types::Value::Text(value) => lua.create_string(value).map(Value::String),
        rusqlite::types::Value::Blob(value) => {
            lua.create_userdata(LuaBlob(value)).map(Value::UserData)
        }
    }
}

fn dense_table_values(table: Table) -> Result<Vec<Value>, ProjectLuaCallError> {
    let mut indexed = Vec::new();
    for pair in table.pairs::<Value, Value>() {
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
        indexed.push((index, value));
    }
    indexed.sort_by_key(|(index, _)| *index);
    for (offset, (index, _)) in indexed.iter().enumerate() {
        if *index != offset + 1 {
            return Err(ProjectLuaCallError::new(
                "invalid_array",
                "数组必须无洞且连续",
            ));
        }
    }
    Ok(indexed.into_iter().map(|(_, value)| value).collect())
}

fn lua_to_project_value(value: Value) -> Result<ProjectLuaValue, ProjectLuaCallError> {
    lua_to_project_value_inner(value, &mut HashSet::new())
}

fn lua_to_project_value_inner(
    value: Value,
    active_tables: &mut HashSet<*const c_void>,
) -> Result<ProjectLuaValue, ProjectLuaCallError> {
    match value {
        Value::Nil => Ok(ProjectLuaValue::Nil),
        Value::Boolean(value) => Ok(ProjectLuaValue::Boolean(value)),
        Value::Integer(value) => Ok(ProjectLuaValue::Integer(value)),
        Value::Number(value) if value.is_finite() => Ok(ProjectLuaValue::Real(value)),
        Value::Number(_) => Err(ProjectLuaCallError::new(
            "invalid_real",
            "translation 参数不得包含 NaN 或 Inf",
        )),
        Value::String(value) => {
            strict_text(&value, "translation 字符串").map(ProjectLuaValue::Text)
        }
        Value::UserData(value) if value.is::<LuaSqliteNull>() => Ok(ProjectLuaValue::Nil),
        Value::UserData(value) if value.is::<LuaBlob>() => Ok(ProjectLuaValue::Blob(
            value
                .borrow::<LuaBlob>()
                .map_err(|error| ProjectLuaCallError::new("invalid_blob", error.to_string()))?
                .0
                .clone(),
        )),
        Value::Table(table) => project_table_value(table, active_tables),
        other => Err(ProjectLuaCallError::new(
            "unsupported_value",
            format!("translation 参数不支持 {}", other.type_name()),
        )),
    }
}

fn project_table_value(
    table: Table,
    active_tables: &mut HashSet<*const c_void>,
) -> Result<ProjectLuaValue, ProjectLuaCallError> {
    let identity = table.to_pointer();
    if !active_tables.insert(identity) {
        return Err(ProjectLuaCallError::new(
            "cyclic_table",
            "translation 参数不得包含循环 table",
        ));
    }

    let mut integer_fields = Vec::new();
    let mut string_fields = BTreeMap::new();
    for pair in table.pairs::<Value, Value>() {
        let (key, value) =
            pair.map_err(|error| ProjectLuaCallError::new("invalid_table", error.to_string()))?;
        let value = lua_to_project_value_inner(value, active_tables)?;
        match key {
            Value::Integer(index) => {
                let index = usize::try_from(index)
                    .ok()
                    .filter(|index| *index > 0)
                    .ok_or_else(|| {
                        ProjectLuaCallError::new("invalid_table", "数组下标必须从 1 开始")
                    })?;
                integer_fields.push((index, value));
            }
            Value::String(key) => {
                let key = strict_text(&key, "translation 字段名")?;
                if string_fields.insert(key, value).is_some() {
                    return Err(ProjectLuaCallError::new(
                        "duplicate_field",
                        "translation table 包含重复字段",
                    ));
                }
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
    }
    active_tables.remove(&identity);

    if !integer_fields.is_empty() && !string_fields.is_empty() {
        return Err(ProjectLuaCallError::new(
            "mixed_table",
            "translation table 不能混用数组下标和字段名",
        ));
    }
    if !string_fields.is_empty() || integer_fields.is_empty() {
        return Ok(ProjectLuaValue::Object(string_fields));
    }

    integer_fields.sort_by_key(|(index, _)| *index);
    for (offset, (index, _)) in integer_fields.iter().enumerate() {
        if *index != offset + 1 {
            return Err(ProjectLuaCallError::new(
                "invalid_array",
                "translation 数组必须无洞且连续",
            ));
        }
    }
    Ok(ProjectLuaValue::Array(
        integer_fields.into_iter().map(|(_, value)| value).collect(),
    ))
}

fn strict_text(value: &mlua::LuaString, role: &str) -> Result<String, ProjectLuaCallError> {
    value.to_str().map(|value| value.to_owned()).map_err(|_| {
        ProjectLuaCallError::new("invalid_utf8", format!("{role} 必须是 UTF-8 字符串"))
    })
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
struct LuaBlob(Vec<u8>);

impl UserData for LuaBlob {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("bytes", |lua, this, ()| lua.create_string(&this.0));
        methods.add_meta_method(MetaMethod::Eq, |_lua, this, other: AnyUserData| {
            if !other.is::<LuaBlob>() {
                return Ok(false);
            }
            Ok(other.borrow::<LuaBlob>()?.0 == this.0)
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

pub(super) fn execute(lua: &Lua, function: Function) -> Result<(), ProjectLuaFailure> {
    let runner: Function = lua
        .load(
            "return function(main) local ok, value = xpcall(main, function(error) return error end); return ok, value end",
        )
        .eval()
        .map_err(|error| ProjectLuaFailure::Context(error.to_string()))?;
    let (succeeded, error): (bool, Value) = runner
        .call(function)
        .map_err(|error| ProjectLuaFailure::Script(error.to_string()))?;
    if succeeded {
        return Ok(());
    }
    if let Value::UserData(userdata) = &error
        && let Ok(error) = userdata.borrow::<LuaHostCallError>()
    {
        return Err(ProjectLuaFailure::Host {
            domain: error.domain,
            kind: error.kind,
            operation: error.operation,
            message: error.message.clone(),
        });
    }
    Err(ProjectLuaFailure::Script(lua_value_description(&error)))
}

fn lua_value_description(value: &Value) -> String {
    match value {
        Value::String(value) => value
            .to_str()
            .map(|value| value.to_owned())
            .unwrap_or_else(|_| escaped_lua_bytes(value.as_bytes().as_ref())),
        Value::Error(error) => error.to_string(),
        other => format!("Lua {} 错误值", other.type_name()),
    }
}

fn escaped_lua_bytes(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut escaped = String::from("非 UTF-8 Lua 错误字符串（原始字节）：");
    for byte in bytes {
        write!(&mut escaped, "\\x{byte:02X}").expect("写入 String 不会失败");
    }
    escaped
}
