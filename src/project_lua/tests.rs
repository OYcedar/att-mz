use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use rusqlite::{
    Connection,
    hooks::{AuthAction, AuthContext, Authorization, TransactionOperation},
};

use super::{
    ProjectLuaCallError, ProjectLuaCancellation, ProjectLuaEngineAdapter, ProjectLuaFailure,
    ProjectLuaPrintSink, ProjectLuaProgram, ProjectLuaProject, ProjectLuaRunError,
    ProjectLuaRunRequest, ProjectLuaSchemaObjectKind, ProjectLuaValue, rollback, run_project_lua,
};

#[derive(Debug, Default)]
struct TestAdapter {
    fail_validation: bool,
}

impl ProjectLuaEngineAdapter for TestAdapter {
    fn protects_schema_object(
        &self,
        kind: ProjectLuaSchemaObjectKind,
        name: &str,
        _table_name: &str,
    ) -> bool {
        kind == ProjectLuaSchemaObjectKind::Table
            && (name.eq_ignore_ascii_case("units") || name.eq_ignore_ascii_case("reserved"))
    }

    fn set_translation(
        &self,
        connection: &Connection,
        locator: ProjectLuaValue,
        translation: ProjectLuaValue,
    ) -> Result<u64, ProjectLuaCallError> {
        let ProjectLuaValue::Object(locator) = locator else {
            return Err(ProjectLuaCallError::new(
                "invalid_locator",
                "locator 必须是 object",
            ));
        };
        let Some(ProjectLuaValue::Text(id)) = locator.get("id") else {
            return Err(ProjectLuaCallError::new(
                "invalid_locator",
                "locator.id 必须是字符串",
            ));
        };
        let ProjectLuaValue::Text(translation) = translation else {
            return Err(ProjectLuaCallError::new(
                "invalid_translation",
                "translation 必须是字符串",
            ));
        };
        let changed = connection
            .execute(
                "UPDATE main.units SET translation = ?1 WHERE id = ?2",
                rusqlite::params![translation, id],
            )
            .map_err(|error| ProjectLuaCallError::new("sqlite", error.to_string()))?;
        if changed != 1 {
            return Err(ProjectLuaCallError::new("unknown_unit", "目标 Unit 不存在"));
        }
        Ok(u64::try_from(changed).expect("受支持平台的 usize 必须能表示为 u64"))
    }

    fn clear_translation(
        &self,
        connection: &Connection,
        locator: ProjectLuaValue,
    ) -> Result<u64, ProjectLuaCallError> {
        let ProjectLuaValue::Object(locator) = locator else {
            return Err(ProjectLuaCallError::new(
                "invalid_locator",
                "locator 必须是 object",
            ));
        };
        let Some(ProjectLuaValue::Text(id)) = locator.get("id") else {
            return Err(ProjectLuaCallError::new(
                "invalid_locator",
                "locator.id 必须是字符串",
            ));
        };
        let changed = connection
            .execute(
                "UPDATE main.units SET translation = NULL WHERE id = ?1",
                [id],
            )
            .map_err(|error| ProjectLuaCallError::new("sqlite", error.to_string()))?;
        Ok(u64::try_from(changed).expect("受支持平台的 usize 必须能表示为 u64"))
    }

    fn validate_database(
        &self,
        _connection: &Connection,
        _project: &ProjectLuaProject,
    ) -> Result<(), ProjectLuaCallError> {
        if self.fail_validation {
            Err(ProjectLuaCallError::new("invalid_project", "测试校验失败"))
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Default)]
struct TestPrintSink {
    lines: Mutex<Vec<Vec<u8>>>,
}

impl ProjectLuaPrintSink for TestPrintSink {
    fn print(&self, bytes: &[u8]) -> Result<(), ProjectLuaCallError> {
        self.lines
            .lock()
            .expect("测试输出锁不应中毒")
            .push(bytes.to_vec());
        Ok(())
    }
}

#[derive(Debug)]
struct SignalPrintSink {
    sender: Mutex<Option<std::sync::mpsc::SyncSender<()>>>,
}

impl ProjectLuaPrintSink for SignalPrintSink {
    fn print(&self, _bytes: &[u8]) -> Result<(), ProjectLuaCallError> {
        if let Some(sender) = self.sender.lock().expect("测试信号锁不应中毒").take() {
            sender.send(()).map_err(|_| {
                ProjectLuaCallError::new("test_signal", "无法发送 Lua 测试启动信号")
            })?;
        }
        Ok(())
    }
}

fn database() -> Connection {
    let connection = Connection::open_in_memory().expect("应建立内存数据库");
    connection
        .execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE units (
               id TEXT PRIMARY KEY,
               translation TEXT
             );
             INSERT INTO units VALUES ('unit-1', NULL);",
        )
        .expect("应建立测试 schema");
    connection
}

fn request(source: &str, adapter: Arc<dyn ProjectLuaEngineAdapter>) -> ProjectLuaRunRequest {
    ProjectLuaRunRequest::new(
        ProjectLuaProject::new("test-project", "generic"),
        ProjectLuaProgram::new("test.lua", source.as_bytes(), vec!["one".to_owned()]),
        adapter,
    )
}

#[test]
fn successful_script_commits_database_values_and_exposes_only_restricted_context() {
    let connection = database();
    let print_sink = Arc::new(TestPrintSink::default());
    let script = r#"
assert(ctx.project.name == "test-project")
assert(ctx.project.engine == "generic")
assert(not pcall(function() ctx.project.name = "changed" end))
assert(arg[0] == "test.lua")
assert(arg[1] == "one")
assert(io == nil and os == nil and package == nil and require == nil)
assert(loadfile == nil and dofile == nil and debug == nil and warn == nil)

ctx.db.execute(
  "CREATE TABLE private_values (n, r, t, b, z)"
)
ctx.db.execute(
  "INSERT INTO private_values VALUES (?1, ?2, ?3, ?4, ?5)",
  {7, 1.5, "text", ctx.db.blob(string.char(0, 255)), ctx.db.NULL}
)
local rows = ctx.db.query("SELECT n, r, t, b, z FROM private_values")
assert(rows[1][1] == 7)
assert(rows[1][2] == 1.5)
assert(rows[1][3] == "text")
assert(rows[1][4]:bytes() == string.char(0, 255))
assert(rows[1][5] == ctx.db.NULL)
print("done", 7)
"#;
    let report = run_project_lua(
        connection,
        request(script, Arc::new(TestAdapter::default())).with_print_sink(print_sink.clone()),
    )
    .expect("脚本应成功");

    assert_eq!(report.database_calls(), 3);
    assert_eq!(report.changed_rows(), 1);
    assert_eq!(report.translation_calls(), 0);
    assert_eq!(report.printed_lines(), 1);
    assert_eq!(
        print_sink
            .lines
            .lock()
            .expect("测试输出锁不应中毒")
            .as_slice(),
        [b"done\t7".to_vec()]
    );
}

#[test]
fn failed_validation_rolls_back_every_change() {
    let temporary = tempfile::tempdir().expect("应建立临时目录");
    let path = temporary.path().join("state.db");
    let connection = Connection::open(&path).expect("应建立数据库");
    connection
        .execute_batch(
            "CREATE TABLE units (id TEXT PRIMARY KEY, translation TEXT);
             INSERT INTO units VALUES ('unit-1', NULL);",
        )
        .expect("应建立测试 schema");

    let error = run_project_lua(
        connection,
        request(
            "ctx.db.execute(\"UPDATE units SET translation = 'changed'\")",
            Arc::new(TestAdapter {
                fail_validation: true,
            }),
        ),
    )
    .expect_err("最终校验失败应回滚");
    assert!(matches!(
        error,
        ProjectLuaRunError::RolledBack(ProjectLuaFailure::Host {
            operation: "translation.validate",
            ..
        })
    ));

    let translation: Option<String> = Connection::open(path)
        .expect("应重开数据库")
        .query_row(
            "SELECT translation FROM units WHERE id = 'unit-1'",
            [],
            |row| row.get(0),
        )
        .expect("应读取回滚后的值");
    assert_eq!(translation, None);
}

#[test]
fn failed_commit_reports_unknown_outcome_after_sqlite_ends_the_transaction() {
    let temporary = tempfile::tempdir().expect("应建立临时目录");
    let path = temporary.path().join("state.db");
    let connection = Connection::open(&path).expect("应建立数据库");
    connection
        .execute_batch(
            "CREATE TABLE units (id TEXT PRIMARY KEY, translation TEXT);
             INSERT INTO units VALUES ('unit-1', NULL);",
        )
        .expect("应建立测试 schema");
    connection
        .commit_hook(Some(|| true))
        .expect("应安装测试 COMMIT hook");

    let error = run_project_lua(
        connection,
        request(
            "ctx.db.execute(\"UPDATE units SET translation = 'changed'\")",
            Arc::new(TestAdapter::default()),
        ),
    )
    .expect_err("SQLite 已结束事务的 COMMIT 失败必须报告未知结果");
    assert!(matches!(
        error,
        ProjectLuaRunError::CommitOutcomeUnknown(ref sqlite_error)
            if sqlite_error.operation == "commit"
    ));

    let translation: Option<String> = Connection::open(path)
        .expect("应重开数据库")
        .query_row(
            "SELECT translation FROM units WHERE id = 'unit-1'",
            [],
            |row| row.get(0),
        )
        .expect("应读取 SQLite 拒绝 COMMIT 后的值");
    assert_eq!(translation, None);
}

#[test]
fn failed_rollback_reports_unknown_outcome_while_transaction_remains_open() {
    let connection = database();
    connection
        .execute_batch("BEGIN IMMEDIATE")
        .expect("应开始测试事务");
    connection
        .authorizer(Some(|context: AuthContext<'_>| {
            if matches!(
                context.action,
                AuthAction::Transaction {
                    operation: TransactionOperation::Rollback
                }
            ) {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }))
        .expect("应安装测试 authorizer");
    let connection = Rc::new(RefCell::new(connection));

    let error = rollback(
        &connection,
        ProjectLuaFailure::Validation("测试主失败".to_owned()),
    )
    .expect_err("ROLLBACK 被 SQLite 拒绝时必须报告未知结果");
    assert!(matches!(
        error,
        ProjectLuaRunError::RollbackOutcomeUnknown {
            failure: ProjectLuaFailure::Validation(ref message),
            ref rollback,
        } if message == "测试主失败" && rollback.operation == "rollback"
    ));
    assert!(!connection.borrow().is_autocommit());
}

#[test]
fn uncaught_error_rolls_back_but_caught_sql_error_can_continue() {
    let temporary = tempfile::tempdir().expect("应建立临时目录");
    let path = temporary.path().join("state.db");
    let connection = Connection::open(&path).expect("应建立数据库");
    connection
        .execute_batch(
            "CREATE TABLE units (id TEXT PRIMARY KEY, translation TEXT);
             INSERT INTO units VALUES ('unit-1', NULL);",
        )
        .expect("应建立测试 schema");

    let caught = r#"
local ok = pcall(ctx.db.execute, "BEGIN")
assert(not ok)
ctx.db.execute("UPDATE units SET translation = 'kept'")
"#;
    run_project_lua(
        connection,
        request(caught, Arc::new(TestAdapter::default())),
    )
    .expect("捕获 SQL 错误后应能继续");
    let kept: String = Connection::open(&path)
        .expect("应重开数据库")
        .query_row(
            "SELECT translation FROM units WHERE id = 'unit-1'",
            [],
            |row| row.get(0),
        )
        .expect("应读取已提交值");
    assert_eq!(kept, "kept");

    let connection = Connection::open(&path).expect("应重开数据库");
    let uncaught = r#"
ctx.db.execute("UPDATE units SET translation = 'rolled-back'")
error("stop")
"#;
    let error = run_project_lua(
        connection,
        request(uncaught, Arc::new(TestAdapter::default())),
    )
    .expect_err("未捕获错误应回滚");
    assert!(matches!(
        error,
        ProjectLuaRunError::RolledBack(ProjectLuaFailure::Script(_))
    ));
    let current: String = Connection::open(path)
        .expect("应再次重开数据库")
        .query_row(
            "SELECT translation FROM units WHERE id = 'unit-1'",
            [],
            |row| row.get(0),
        )
        .expect("应读取回滚后的值");
    assert_eq!(current, "kept");
}

#[test]
fn rollback_conflict_cannot_escape_outer_transaction_through_pcall() {
    let temporary = tempfile::tempdir().expect("应建立临时目录");
    let path = temporary.path().join("state.db");
    let connection = Connection::open(&path).expect("应建立数据库");
    connection
        .execute_batch(
            "CREATE TABLE units (id TEXT PRIMARY KEY, translation TEXT);
             INSERT INTO units VALUES ('unit-1', NULL);",
        )
        .expect("应建立测试 schema");

    let script = r#"
ctx.db.execute("UPDATE units SET translation = 'before-rollback'")
local rollback_ok = pcall(
  ctx.db.execute,
  "INSERT OR ROLLBACK INTO units (id, translation) VALUES ('unit-1', 'duplicate')"
)
assert(not rollback_ok)
local escaped_write_ok = pcall(
  ctx.db.execute,
  "UPDATE units SET translation = 'escaped-autocommit'"
)
assert(not escaped_write_ok)
"#;
    let error = run_project_lua(
        connection,
        request(script, Arc::new(TestAdapter::default())),
    )
    .expect_err("提前结束外层事务必须让整个脚本失败");
    assert!(matches!(
        error,
        ProjectLuaRunError::RolledBack(ProjectLuaFailure::Host {
            domain: "database",
            kind: "transaction_lost",
            operation: "transaction",
            ..
        })
    ));

    let translation: Option<String> = Connection::open(path)
        .expect("应重开数据库")
        .query_row(
            "SELECT translation FROM units WHERE id = 'unit-1'",
            [],
            |row| row.get(0),
        )
        .expect("应读取未被自动提交污染的值");
    assert_eq!(translation, None);
}

#[test]
fn multiple_statements_attach_and_att_schema_changes_are_denied() {
    let script = r#"
for _, sql in ipairs({
  "SELECT 1; SELECT 2",
  "ATTACH DATABASE ':memory:' AS other",
  "DROP TABLE units",
  "CREATE INDEX unit_translation ON units(translation)",
  "CREATE TABLE reserved (value TEXT)",
  "CREATE TEMP TABLE UnItS (id TEXT)",
  "CREATE TEMP VIEW units AS SELECT 1 AS id",
  "PRAGMA user_version = 4"
}) do
  local ok = pcall(ctx.db.execute, sql)
  assert(not ok)
end
local returning_ok = pcall(
  ctx.db.query,
  "UPDATE main.units SET translation = 'hidden-write' RETURNING id"
)
assert(not returning_ok)
local unchanged = ctx.db.query(
  "SELECT translation FROM main.units WHERE id = 'unit-1'"
)
assert(unchanged[1][1] == ctx.db.NULL)
ctx.db.execute("CREATE TABLE private_table (value TEXT)")
ctx.db.execute("CREATE TEMP TABLE private_temp (value TEXT)")
"#;
    run_project_lua(
        database(),
        request(script, Arc::new(TestAdapter::default())),
    )
    .expect("禁止的 SQL 被捕获后，私有 DDL 应提交");
}

#[test]
fn temp_rename_cannot_shadow_an_att_table() {
    let error = run_project_lua(
        database(),
        request(
            r#"
ctx.db.execute("CREATE TEMP TABLE private_shadow (id TEXT)")
ctx.db.execute("ALTER TABLE temp.private_shadow RENAME TO UnItS")
"#,
            Arc::new(TestAdapter::default()),
        ),
    )
    .expect_err("TEMP rename 不能绕过 ATT 名称保护");

    assert!(matches!(
        error,
        ProjectLuaRunError::RolledBack(ProjectLuaFailure::Validation(_))
    ));
}

#[test]
fn execute_reports_only_direct_changed_rows() {
    let connection = database();
    connection
        .execute_batch(
            "CREATE TABLE private_change_log (unit_id TEXT NOT NULL);
             CREATE TRIGGER units_change_log
             AFTER UPDATE ON units
             BEGIN
               INSERT INTO private_change_log VALUES (NEW.id);
             END;",
        )
        .expect("应建立测试 trigger");

    let report = run_project_lua(
        connection,
        request(
            r#"
local changed = ctx.db.execute(
  "UPDATE main.units SET translation = 'changed' WHERE id = 'unit-1'"
)
assert(changed == 1)
local trigger_rows = ctx.db.query("SELECT unit_id FROM main.private_change_log")
assert(#trigger_rows == 1 and trigger_rows[1][1] == "unit-1")
"#,
            Arc::new(TestAdapter::default()),
        ),
    )
    .expect("直接改动和 trigger 改动都应提交");

    assert_eq!(report.changed_rows(), 1);
}

#[test]
fn waiting_for_database_write_lock_observes_cancellation() {
    let temporary = tempfile::tempdir().expect("应建立临时目录");
    let path = temporary.path().join("busy.db");
    let setup = Connection::open(&path).expect("应建立数据库");
    setup
        .execute_batch(
            "CREATE TABLE units (id TEXT PRIMARY KEY, translation TEXT);
             INSERT INTO units VALUES ('unit-1', NULL);",
        )
        .expect("应建立测试 schema");
    drop(setup);

    let locker = Connection::open(&path).expect("应打开锁持有连接");
    locker
        .execute_batch(
            "BEGIN IMMEDIATE;
             UPDATE units SET translation = 'locked' WHERE id = 'unit-1';",
        )
        .expect("应取得写锁");

    let cancellation = ProjectLuaCancellation::default();
    let worker_cancellation = cancellation.clone();
    let worker_path = path.clone();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let connection = Connection::open(worker_path).expect("worker 应打开数据库");
        let result = run_project_lua(
            connection,
            request(
                "ctx.db.execute(\"UPDATE main.units SET translation = 'worker'\")",
                Arc::new(TestAdapter::default()),
            )
            .with_cancellation(worker_cancellation),
        );
        let _ = sender.send(result);
    });

    std::thread::sleep(std::time::Duration::from_millis(50));
    cancellation.cancel();
    let result = receiver.recv_timeout(std::time::Duration::from_secs(2));
    locker.execute_batch("ROLLBACK").expect("应释放测试写锁");
    worker.join().expect("Lua worker 不应 panic");

    let error = result
        .expect("等待 SQLite 写锁的 Lua 应在取消后及时返回")
        .expect_err("取消等待必须失败");
    assert_eq!(
        error,
        ProjectLuaRunError::NotStarted(ProjectLuaFailure::Cancelled)
    );
}

#[test]
fn cancellation_after_commit_starts_does_not_interrupt_finalization() {
    struct BusySignalGuard;

    impl Drop for BusySignalGuard {
        fn drop(&mut self) {
            *super::PROJECT_LUA_FINALIZING_BUSY_TEST_SIGNAL
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        }
    }

    let temporary = tempfile::tempdir().expect("应建立临时目录");
    let path = temporary.path().join("commit.db");
    let setup = Connection::open(&path).expect("应建立数据库");
    setup
        .execute_batch(
            "CREATE TABLE units (id TEXT PRIMARY KEY, translation TEXT);
             INSERT INTO units VALUES ('unit-1', NULL);",
        )
        .expect("应建立测试 schema");
    drop(setup);

    let reader = Connection::open(&path).expect("应打开读事务连接");
    reader.execute_batch("BEGIN").expect("应开始读事务");
    let _: Option<String> = reader
        .query_row(
            "SELECT translation FROM units WHERE id = 'unit-1'",
            [],
            |row| row.get(0),
        )
        .expect("应取得共享读锁");

    let cancellation = ProjectLuaCancellation::default();
    let worker_cancellation = cancellation.clone();
    let worker_path = path.clone();
    let (busy_sender, busy_receiver) = std::sync::mpsc::sync_channel(1);
    *super::PROJECT_LUA_FINALIZING_BUSY_TEST_SIGNAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(busy_sender);
    let _busy_signal_guard = BusySignalGuard;
    let (result_sender, result_receiver) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let connection = Connection::open(worker_path).expect("worker 应打开数据库");
        let result = run_project_lua(
            connection,
            request(
                "ctx.db.execute(\"UPDATE main.units SET translation = 'committed'\")",
                Arc::new(TestAdapter::default()),
            )
            .with_cancellation(worker_cancellation),
        );
        let _ = result_sender.send(result);
    });

    busy_receiver
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("Lua COMMIT 应在超时前等待共享读锁");
    cancellation.cancel();
    reader.execute_batch("ROLLBACK").expect("应释放共享读锁");

    result_receiver
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("COMMIT 应在读锁释放后返回明确结果")
        .expect("COMMIT 开始后的取消不能打断最终提交");
    worker.join().expect("Lua worker 不应 panic");
    let translation: String = Connection::open(path)
        .expect("应重开数据库")
        .query_row(
            "SELECT translation FROM units WHERE id = 'unit-1'",
            [],
            |row| row.get(0),
        )
        .expect("应读取提交后的译文");
    assert_eq!(translation, "committed");
}

#[test]
fn translation_adapter_receives_owned_locator_and_value_in_same_transaction() {
    let temporary = tempfile::tempdir().expect("应建立临时目录");
    let path = temporary.path().join("state.db");
    let connection = Connection::open(&path).expect("应建立数据库");
    connection
        .execute_batch(
            "CREATE TABLE units (id TEXT PRIMARY KEY, translation TEXT);
             INSERT INTO units VALUES ('unit-1', NULL);",
        )
        .expect("应建立测试 schema");
    let script = r#"
ctx.translation.set({id = "unit-1"}, "first")
ctx.translation.clear({id = "unit-1"})
ctx.translation.set({id = "unit-1"}, "final")
"#;
    let report = run_project_lua(
        connection,
        request(script, Arc::new(TestAdapter::default())),
    )
    .expect("译文操作应成功");
    assert_eq!(report.translation_calls(), 3);

    let translation: String = Connection::open(path)
        .expect("应重开数据库")
        .query_row(
            "SELECT translation FROM units WHERE id = 'unit-1'",
            [],
            |row| row.get(0),
        )
        .expect("应读取最终译文");
    assert_eq!(translation, "final");
}

#[test]
fn cancellation_before_start_does_not_begin_a_transaction() {
    let cancellation = ProjectLuaCancellation::default();
    cancellation.cancel();
    let error = run_project_lua(
        database(),
        request("while true do end", Arc::new(TestAdapter::default()))
            .with_cancellation(cancellation),
    )
    .expect_err("预先取消必须直接返回");
    assert_eq!(
        error,
        ProjectLuaRunError::NotStarted(ProjectLuaFailure::Cancelled)
    );
}

#[test]
fn running_lua_loop_observes_cross_thread_cancellation() {
    let cancellation = ProjectLuaCancellation::default();
    let worker_cancellation = cancellation.clone();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let sink = Arc::new(SignalPrintSink {
        sender: Mutex::new(Some(sender)),
    });
    let worker = std::thread::spawn(move || {
        run_project_lua(
            database(),
            request(
                "print('ready'); while true do end",
                Arc::new(TestAdapter::default()),
            )
            .with_cancellation(worker_cancellation)
            .with_print_sink(sink),
        )
    });
    let started = receiver.recv_timeout(std::time::Duration::from_secs(2));
    cancellation.cancel();
    started.expect("Lua 应在超时前开始运行");
    let error = worker
        .join()
        .expect("Lua worker 不应 panic")
        .expect_err("无限循环应被取消");
    assert_eq!(
        error,
        ProjectLuaRunError::RolledBack(ProjectLuaFailure::Cancelled)
    );
}

#[test]
fn invalid_utf8_script_is_rejected_before_transaction() {
    let request = ProjectLuaRunRequest::new(
        ProjectLuaProject::new("test-project", "generic"),
        ProjectLuaProgram::new("invalid.lua", vec![0xff], Vec::new()),
        Arc::new(TestAdapter::default()),
    );
    let error = run_project_lua(database(), request).expect_err("无效 UTF-8 必须被拒绝");
    assert!(matches!(
        error,
        ProjectLuaRunError::NotStarted(ProjectLuaFailure::Compile(_))
    ));
}

#[test]
fn project_value_distinguishes_object_array_blob_and_scalars() {
    #[derive(Debug, Default)]
    struct InspectingAdapter {
        seen: Mutex<Option<(ProjectLuaValue, ProjectLuaValue)>>,
    }

    impl ProjectLuaEngineAdapter for InspectingAdapter {
        fn protects_schema_object(
            &self,
            kind: ProjectLuaSchemaObjectKind,
            name: &str,
            _table_name: &str,
        ) -> bool {
            kind == ProjectLuaSchemaObjectKind::Table && name == "units"
        }

        fn set_translation(
            &self,
            _connection: &Connection,
            locator: ProjectLuaValue,
            translation: ProjectLuaValue,
        ) -> Result<u64, ProjectLuaCallError> {
            *self.seen.lock().expect("测试锁不应中毒") = Some((locator, translation));
            Ok(1)
        }

        fn clear_translation(
            &self,
            _connection: &Connection,
            _locator: ProjectLuaValue,
        ) -> Result<u64, ProjectLuaCallError> {
            Ok(1)
        }

        fn validate_database(
            &self,
            _connection: &Connection,
            _project: &ProjectLuaProject,
        ) -> Result<(), ProjectLuaCallError> {
            Ok(())
        }
    }

    let adapter = Arc::new(InspectingAdapter::default());
    run_project_lua(
        database(),
        request(
            r#"ctx.translation.set(
              {id = "unit-1", ordinal = 3, nested = {"a", true}},
              ctx.db.blob(string.char(0, 255))
            )"#,
            adapter.clone(),
        ),
    )
    .expect("通用值应交给适配器");

    let mut expected_locator = BTreeMap::new();
    expected_locator.insert("id".to_owned(), ProjectLuaValue::Text("unit-1".to_owned()));
    expected_locator.insert("ordinal".to_owned(), ProjectLuaValue::Integer(3));
    expected_locator.insert(
        "nested".to_owned(),
        ProjectLuaValue::Array(vec![
            ProjectLuaValue::Text("a".to_owned()),
            ProjectLuaValue::Boolean(true),
        ]),
    );
    assert_eq!(
        adapter.seen.lock().expect("测试锁不应中毒").as_ref(),
        Some(&(
            ProjectLuaValue::Object(expected_locator),
            ProjectLuaValue::Blob(vec![0, 255]),
        ))
    );
}
