use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rusqlite::{
    Connection,
    hooks::{AuthAction, AuthContext, Authorization, TransactionOperation},
};
use sha2::{Digest, Sha256};

use crate::fingerprint::Sha256Fingerprint;

use super::{
    ProjectLuaCallError, ProjectLuaCancellation, ProjectLuaCompilationFailure,
    ProjectLuaDatabasePrerequisiteError, ProjectLuaEngineAdapter, ProjectLuaFailure,
    ProjectLuaPrintSink, ProjectLuaProgram, ProjectLuaProject, ProjectLuaRunError,
    ProjectLuaRunRequest, ProjectLuaSchemaObjectKind, ProjectLuaScriptFailure,
    ProjectLuaSqliteError, ProjectLuaSqliteOperation, ProjectLuaValidationFailure, ProjectLuaValue,
    compile_project_lua_program, compile_project_lua_program_with_cancellation,
    fingerprint_project_lua_program_with_cancellation, rollback, run_project_lua,
    take_project_lua_object_field,
};

fn sqlite_diagnostic_json(operation: &str, transaction: &str, query_id: &str) -> serde_json::Value {
    serde_json::json!({
        "code": "sqlite.driver",
        "stage": "lua",
        "issue": {
            "family": "sqlite",
            "details": {
                "context": {
                    "stage": "lua",
                    "operation": operation,
                    "transaction": transaction
                },
                "problem": {
                    "kind": "driver",
                    "database": "project.db",
                    "query_id": query_id,
                    "query_ordinal": null,
                    "failure": {
                        "kind": "execute_returned_rows",
                        "primary_code": null,
                        "extended_code": null,
                        "column_index": null,
                        "column_name": null,
                        "parameter_actual": null,
                        "parameter_expected": null,
                        "changed_rows": null,
                        "sql_offset": null,
                        "database_index": null
                    }
                }
            }
        },
        "resolution": "retry"
    })
}

#[test]
fn incomplete_external_locator_identifiers_do_not_panic_or_enter_diagnostic_wire() {
    let error =
        ProjectLuaCallError::violation(crate::diagnostic::LuaValueViolation::InvalidLocator)
            .with_field("field\u{0000}")
            .with_generic_locator(
                Some(std::path::Path::new("data/dialogue.jsonl")),
                "group\u{0001}",
                "unit\u{0002}",
            );
    let report = ProjectLuaRunError::NotStarted(ProjectLuaFailure::Host(error))
        .diagnostic_report(std::path::Path::new("project.db"));
    let wire = serde_json::to_string(&report).expect("诊断必须可序列化");

    assert!(!wire.contains("field\\u0000"));
    assert!(!wire.contains("group\\u0001"));
    assert!(!wire.contains("unit\\u0002"));
    assert!(!wire.contains("data/dialogue.jsonl"));
}

#[test]
fn diagnostic_report_serializes_all_project_lua_transaction_outcomes() {
    let database_path = std::path::Path::new("project.db");

    let not_started =
        ProjectLuaRunError::NotStarted(ProjectLuaFailure::Database(ProjectLuaSqliteError::new(
            ProjectLuaSqliteOperation::ReadCurrentAttSchema,
            rusqlite::Error::ExecuteReturnedResults,
        )));
    assert_eq!(
        serde_json::to_value(not_started.diagnostic_report(database_path))
            .expect("未开始报告应可序列化"),
        serde_json::json!({
            "effect": "unchanged",
            "primary": sqlite_diagnostic_json(
                "query",
                "not_started",
                "read_current_att_schema"
            ),
            "related": []
        })
    );

    let rolled_back =
        ProjectLuaRunError::RolledBack(ProjectLuaFailure::Database(ProjectLuaSqliteError::new(
            ProjectLuaSqliteOperation::ReadCurrentAttSchema,
            rusqlite::Error::ExecuteReturnedResults,
        )));
    assert_eq!(
        serde_json::to_value(rolled_back.diagnostic_report(database_path))
            .expect("已回滚报告应可序列化"),
        serde_json::json!({
            "effect": "unchanged",
            "primary": sqlite_diagnostic_json(
                "query",
                "rolled_back",
                "read_current_att_schema"
            ),
            "related": []
        })
    );

    let rollback_unknown = ProjectLuaRunError::RollbackOutcomeUnknown {
        failure: ProjectLuaFailure::Database(ProjectLuaSqliteError::new(
            ProjectLuaSqliteOperation::ReadCurrentAttSchema,
            rusqlite::Error::ExecuteReturnedResults,
        )),
        rollback: ProjectLuaSqliteError::new(
            ProjectLuaSqliteOperation::Rollback,
            rusqlite::Error::ExecuteReturnedResults,
        ),
    };
    assert_eq!(
        serde_json::to_value(rollback_unknown.diagnostic_report(database_path))
            .expect("回滚结果未知报告应可序列化"),
        serde_json::json!({
            "effect": "outcome_unknown",
            "primary": sqlite_diagnostic_json(
                "query",
                "outcome_unknown",
                "read_current_att_schema"
            ),
            "related": [{
                "relation": "rollback",
                "report": {
                    "effect": "outcome_unknown",
                    "primary": sqlite_diagnostic_json(
                        "transaction",
                        "outcome_unknown",
                        "rollback"
                    ),
                    "related": []
                }
            }]
        })
    );

    let commit_unknown = ProjectLuaRunError::CommitOutcomeUnknown(ProjectLuaSqliteError::new(
        ProjectLuaSqliteOperation::Commit,
        rusqlite::Error::ExecuteReturnedResults,
    ));
    assert_eq!(
        serde_json::to_value(commit_unknown.diagnostic_report(database_path))
            .expect("提交结果未知报告应可序列化"),
        serde_json::json!({
            "effect": "outcome_unknown",
            "primary": sqlite_diagnostic_json(
                "transaction",
                "outcome_unknown",
                "commit"
            ),
            "related": []
        })
    );
}

#[derive(Debug, Default)]
struct TestAdapter {
    prerequisite_failure: Option<ProjectLuaDatabasePrerequisiteError>,
    fail_validation: bool,
    cancel_after_translation: Option<ProjectLuaCancellation>,
    observed_translation_calls: Option<Arc<AtomicUsize>>,
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
        let Some(mut locator) = locator.into_object() else {
            return Err(ProjectLuaCallError::violation(
                crate::diagnostic::LuaValueViolation::InvalidLocator,
            ));
        };
        let Some(id) =
            take_project_lua_object_field(&mut locator, "id").and_then(ProjectLuaValue::into_text)
        else {
            return Err(ProjectLuaCallError::violation(
                crate::diagnostic::LuaValueViolation::InvalidLocator,
            )
            .with_field("id"));
        };
        let Some(translation) = translation.into_text() else {
            return Err(ProjectLuaCallError::violation(
                crate::diagnostic::LuaValueViolation::InvalidTranslation,
            ));
        };
        let changed = connection
            .execute(
                "UPDATE main.units SET translation = ?1 WHERE id = ?2",
                rusqlite::params![translation, id],
            )
            .map_err(ProjectLuaCallError::sqlite)?;
        if changed != 1 {
            return Err(ProjectLuaCallError::violation(
                crate::diagnostic::LuaValueViolation::UnknownUnit,
            ));
        }
        if let Some(calls) = &self.observed_translation_calls {
            calls.fetch_add(1, Ordering::SeqCst);
        }
        if let Some(cancellation) = &self.cancel_after_translation {
            cancellation.cancel();
        }
        Ok(u64::try_from(changed).expect("受支持平台的 usize 必须能表示为 u64"))
    }

    fn clear_translation(
        &self,
        connection: &Connection,
        locator: ProjectLuaValue,
    ) -> Result<u64, ProjectLuaCallError> {
        let Some(mut locator) = locator.into_object() else {
            return Err(ProjectLuaCallError::violation(
                crate::diagnostic::LuaValueViolation::InvalidLocator,
            ));
        };
        let Some(id) =
            take_project_lua_object_field(&mut locator, "id").and_then(ProjectLuaValue::into_text)
        else {
            return Err(ProjectLuaCallError::violation(
                crate::diagnostic::LuaValueViolation::InvalidLocator,
            )
            .with_field("id"));
        };
        let changed = connection
            .execute(
                "UPDATE main.units SET translation = NULL WHERE id = ?1",
                [id],
            )
            .map_err(ProjectLuaCallError::sqlite)?;
        Ok(u64::try_from(changed).expect("受支持平台的 usize 必须能表示为 u64"))
    }

    fn validate_database_before_script(
        &self,
        _connection: &Connection,
        _project: &ProjectLuaProject,
    ) -> Result<(), ProjectLuaDatabasePrerequisiteError> {
        match &self.prerequisite_failure {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }

    fn validate_database(
        &self,
        _connection: &Connection,
        _project: &ProjectLuaProject,
    ) -> Result<(), ProjectLuaCallError> {
        if self.fail_validation {
            Err(ProjectLuaCallError::violation(
                crate::diagnostic::LuaValueViolation::StateMismatch,
            ))
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
                ProjectLuaCallError::violation(crate::diagnostic::LuaValueViolation::StateMismatch)
            })?;
        }
        Ok(())
    }
}

#[derive(Debug)]
struct CancellingPrintSink {
    cancellation: ProjectLuaCancellation,
    lines: Mutex<Vec<Vec<u8>>>,
}

impl ProjectLuaPrintSink for CancellingPrintSink {
    fn print(&self, bytes: &[u8]) -> Result<(), ProjectLuaCallError> {
        self.lines
            .lock()
            .expect("测试输出锁不应中毒")
            .push(bytes.to_vec());
        self.cancellation.cancel();
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

fn large_lua_comment(length: usize) -> Vec<u8> {
    let mut source = Vec::with_capacity(length.max(2));
    source.extend_from_slice(b"--");
    source.resize(length.max(2), b'x');
    source
}

fn run_script_cancelled_by_first_print(source: &str) -> (ProjectLuaRunError, Vec<Vec<u8>>) {
    let cancellation = ProjectLuaCancellation::default();
    let sink = Arc::new(CancellingPrintSink {
        cancellation: cancellation.clone(),
        lines: Mutex::new(Vec::new()),
    });
    let worker_sink = Arc::clone(&sink);
    let source = source.to_owned();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let result = run_project_lua(
            database(),
            request(&source, Arc::new(TestAdapter::default()))
                .with_cancellation(cancellation)
                .with_print_sink(worker_sink),
        );
        sender.send(result).expect("测试结果接收端不应提前关闭");
    });
    let result = receiver
        .recv_timeout(std::time::Duration::from_secs(3))
        .expect("取消后的有限脚本必须在超时前结束");
    worker.join().expect("Lua worker 不应 panic");
    let error = result.expect_err("print 触发的取消必须使脚本失败");
    let lines = sink.lines.lock().expect("测试输出锁不应中毒").clone();
    (error, lines)
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
                ..TestAdapter::default()
            }),
        ),
    )
    .expect_err("最终校验失败应回滚");
    assert!(matches!(
        error,
        ProjectLuaRunError::RolledBack(ProjectLuaFailure::Host(error))
            if error.operation() == Some("translation.validate")
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
fn invalid_project_state_prerequisite_keeps_its_typed_failure() {
    let error = run_project_lua(
        database(),
        request(
            "ctx.db.execute(\"UPDATE units SET translation = 'must-not-run'\")",
            Arc::new(TestAdapter {
                prerequisite_failure: Some(
                    ProjectLuaDatabasePrerequisiteError::invalid_project_state(
                        crate::diagnostic::LuaEngine::Generic,
                        crate::diagnostic::LuaValueViolation::StateMismatch,
                    ),
                ),
                ..TestAdapter::default()
            }),
        ),
    )
    .expect_err("无效项目状态必须在脚本执行前失败");

    assert_eq!(
        error,
        ProjectLuaRunError::RolledBack(ProjectLuaFailure::DatabasePrerequisite(
            ProjectLuaDatabasePrerequisiteError::InvalidProjectState {
                engine: crate::diagnostic::LuaEngine::Generic,
                violation: crate::diagnostic::LuaValueViolation::StateMismatch,
            }
        ))
    );
}

#[test]
fn sqlite_prerequisite_preserves_primary_and_extended_codes() {
    let source_connection = Connection::open_in_memory().expect("应建立错误来源数据库");
    source_connection
        .execute_batch(
            "CREATE TABLE private_secret (value TEXT UNIQUE);
             INSERT INTO private_secret VALUES ('sensitive-value');",
        )
        .expect("应建立唯一约束");
    let source = source_connection
        .execute("INSERT INTO private_secret VALUES ('sensitive-value')", [])
        .expect_err("重复值应产生扩展 SQLite code");
    let prerequisite = ProjectLuaDatabasePrerequisiteError::sqlite(
        ProjectLuaSqliteOperation::ReadCurrentAttSchema,
        source,
    );

    let error = run_project_lua(
        database(),
        request(
            "error('must-not-run')",
            Arc::new(TestAdapter {
                prerequisite_failure: Some(prerequisite),
                ..TestAdapter::default()
            }),
        ),
    )
    .expect_err("SQLite 前置检查失败必须保留为数据库错误");

    let ProjectLuaRunError::RolledBack(ProjectLuaFailure::DatabasePrerequisite(
        ProjectLuaDatabasePrerequisiteError::Sqlite(source),
    )) = error
    else {
        panic!("应保留 typed SQLite 前置检查失败");
    };
    assert_eq!(
        source.operation(),
        ProjectLuaSqliteOperation::ReadCurrentAttSchema
    );
    assert_eq!(source.sqlite_codes(), Some((19, 2067)));
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
            if sqlite_error.operation == ProjectLuaSqliteOperation::Commit
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
        ProjectLuaFailure::Validation {
            engine: crate::diagnostic::LuaEngine::Generic,
            failure: ProjectLuaValidationFailure::AdapterState,
        },
    )
    .expect_err("ROLLBACK 被 SQLite 拒绝时必须报告未知结果");
    assert!(matches!(
        error,
        ProjectLuaRunError::RollbackOutcomeUnknown {
            failure: ProjectLuaFailure::Validation {
                engine: crate::diagnostic::LuaEngine::Generic,
                failure: ProjectLuaValidationFailure::AdapterState,
            },
            ref rollback,
        } if rollback.operation == ProjectLuaSqliteOperation::Rollback
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
        ProjectLuaRunError::RolledBack(ProjectLuaFailure::Script { .. })
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
        ProjectLuaRunError::RolledBack(ProjectLuaFailure::Host(error))
            if error.kind() == "transaction_lost"
                && error.operation() == Some("transaction.rollback")
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
        ProjectLuaRunError::RolledBack(ProjectLuaFailure::Validation { .. })
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
fn failed_translation_host_call_is_included_in_failure_metrics() {
    let request = request(
        r#"ctx.translation.set({id = "missing"}, "译文")"#,
        Arc::new(TestAdapter::default()),
    );
    let metrics = request.metrics();

    let error = run_project_lua(database(), request)
        .expect_err("不存在的 Unit 必须让 translation.set 失败并回滚");
    assert!(matches!(
        error,
        ProjectLuaRunError::RolledBack(ProjectLuaFailure::Host(error))
            if error.operation() == Some("translation.set")
    ));
    let report = metrics.report();
    assert_eq!(report.translation_calls(), 1);
    assert_eq!(report.changed_rows(), 0);
}

#[test]
fn cancellation_after_translation_change_keeps_call_and_changed_row_metrics() {
    let cancellation = ProjectLuaCancellation::default();
    let request = request(
        r#"ctx.translation.set({id = "unit-1"}, "译文")"#,
        Arc::new(TestAdapter {
            cancel_after_translation: Some(cancellation.clone()),
            ..TestAdapter::default()
        }),
    )
    .with_cancellation(cancellation);
    let metrics = request.metrics();

    let error =
        run_project_lua(database(), request).expect_err("数据库已经改动后观察到取消，事务必须回滚");
    assert_eq!(
        error,
        ProjectLuaRunError::RolledBack(ProjectLuaFailure::Cancelled)
    );
    let report = metrics.report();
    assert_eq!(report.translation_calls(), 1);
    assert_eq!(report.changed_rows(), 1);
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
fn pcall_and_xpcall_cannot_swallow_cancellation() {
    for (name, script) in [
        (
            "pcall",
            r#"
local ok = pcall(function()
  print("pcall-started")
  local total = 0
  for value = 1, 200000 do total = total + value end
end)
print(ok and "pcall-completed" or "pcall-caught")
"#,
        ),
        (
            "xpcall",
            r#"
local ok = xpcall(
  function()
    print("xpcall-started")
    local total = 0
    for value = 1, 200000 do total = total + value end
  end,
  function(_) return "caught" end
)
print(ok and "xpcall-completed" or "xpcall-caught")
"#,
        ),
    ] {
        let (error, lines) = run_script_cancelled_by_first_print(script);
        assert_eq!(
            error,
            ProjectLuaRunError::RolledBack(ProjectLuaFailure::Cancelled),
            "{name} 不得改变取消结果"
        );
        assert_eq!(
            lines,
            [format!("{name}-started").into_bytes()],
            "{name} 不得捕获取消后继续运行"
        );
    }
}

#[test]
fn cancellation_crosses_nested_resume_and_wrap_coroutines() {
    for (name, script) in [
        (
            "resume",
            r#"
local worker = coroutine.create(function()
  print("resume-started")
  local total = 0
  for value = 1, 200000 do total = total + value end
  print("resume-child-completed")
end)
coroutine.resume(worker)
"#,
        ),
        (
            "wrap",
            r#"
local worker = coroutine.wrap(function()
  print("wrap-started")
  local total = 0
  for value = 1, 200000 do total = total + value end
  print("wrap-child-completed")
end)
worker()
"#,
        ),
    ] {
        let (error, lines) = run_script_cancelled_by_first_print(script);
        assert_eq!(
            error,
            ProjectLuaRunError::RolledBack(ProjectLuaFailure::Cancelled),
            "coroutine.{name} 不得改变取消结果"
        );
        assert_eq!(
            lines,
            [format!("{name}-started").into_bytes()],
            "coroutine.{name} 的子 coroutine 不得忽略全局取消 hook"
        );
    }
}

#[test]
fn cancellation_escapes_non_yieldable_c_boundary() {
    let cancellation = ProjectLuaCancellation::default();
    let observed_translation_calls = Arc::new(AtomicUsize::new(0));
    let error = run_project_lua(
        database(),
        request(
            r#"
pcall(function()
  ctx.translation.set({id = "unit-1"}, "cancel")
  table.sort({3, 2, 1}, function(left, right)
    local total = 0
    for value = 1, 200000 do total = total + value end
    ctx.translation.set({id = "unit-1"}, "escaped")
    return left < right
  end)
end)
"#,
            Arc::new(TestAdapter {
                cancel_after_translation: Some(cancellation.clone()),
                observed_translation_calls: Some(Arc::clone(&observed_translation_calls)),
                ..TestAdapter::default()
            }),
        )
        .with_cancellation(cancellation),
    )
    .expect_err("不可 yield 的比较器必须观察取消");

    assert_eq!(
        error,
        ProjectLuaRunError::RolledBack(ProjectLuaFailure::Cancelled)
    );
    assert_eq!(
        observed_translation_calls.load(Ordering::SeqCst),
        1,
        "取消必须在 table.sort 比较器结束前退出不可 yield 的 C 边界"
    );
}

#[test]
fn cancellation_stops_short_lived_coroutine_creation_inside_pcall() {
    let cancellation = ProjectLuaCancellation::default();
    let observed_translation_calls = Arc::new(AtomicUsize::new(0));
    let error = run_project_lua(
        database(),
        request(
            r#"
ctx.translation.set({id = "unit-1"}, "cancel")
for attempt = 1, 50000 do
  pcall(function()
    local worker = coroutine.create(function() return attempt end)
    coroutine.resume(worker)
  end)
end
ctx.translation.set({id = "unit-1"}, "escaped")
"#,
            Arc::new(TestAdapter {
                cancel_after_translation: Some(cancellation.clone()),
                observed_translation_calls: Some(Arc::clone(&observed_translation_calls)),
                ..TestAdapter::default()
            }),
        )
        .with_cancellation(cancellation),
    )
    .expect_err("短命 coroutine 不得绕过取消检查");

    assert_eq!(
        error,
        ProjectLuaRunError::RolledBack(ProjectLuaFailure::Cancelled)
    );
    assert_eq!(
        observed_translation_calls.load(Ordering::SeqCst),
        1,
        "取消后不得继续创建短命 coroutine 并到达后续 Host 调用"
    );
}

#[test]
fn resumed_child_cannot_print_after_cancellation_yield() {
    let cancellation = ProjectLuaCancellation::default();
    let observed_translation_calls = Arc::new(AtomicUsize::new(0));
    let print_sink = Arc::new(TestPrintSink::default());
    let error = run_project_lua(
        database(),
        request(
            r#"
local worker = coroutine.create(function()
  ctx.translation.set({id = "unit-1"}, "cancel")
  local total = 0
  for value = 1, 200000 do total = total + value end
end)
coroutine.resume(worker)
print("after-resume")
ctx.translation.set({id = "unit-1"}, "escaped")
"#,
            Arc::new(TestAdapter {
                cancel_after_translation: Some(cancellation.clone()),
                observed_translation_calls: Some(Arc::clone(&observed_translation_calls)),
                ..TestAdapter::default()
            }),
        )
        .with_cancellation(cancellation)
        .with_print_sink(print_sink.clone()),
    )
    .expect_err("子 coroutine yield 后父 coroutine 必须立即停止");

    assert_eq!(
        error,
        ProjectLuaRunError::RolledBack(ProjectLuaFailure::Cancelled)
    );
    assert_eq!(observed_translation_calls.load(Ordering::SeqCst), 1);
    assert!(
        print_sink
            .lines
            .lock()
            .expect("测试输出锁不应中毒")
            .is_empty(),
        "coroutine.resume 返回父 coroutine 后不得执行 print 副作用"
    );
}

#[test]
fn ordinary_protected_errors_remain_catchable() {
    let report = run_project_lua(
        database(),
        request(
            r#"
local pcall_ok, pcall_error = pcall(function() error("pcall-error") end)
assert(not pcall_ok and string.find(pcall_error, "pcall-error", 1, true))
local xpcall_ok, xpcall_error = xpcall(
  function() error("xpcall-error") end,
  function(error) return "handled:" .. error end
)
assert(not xpcall_ok and string.find(xpcall_error, "handled:", 1, true))
ctx.db.execute("UPDATE units SET translation = 'caught'")
"#,
            Arc::new(TestAdapter::default()),
        ),
    )
    .expect("普通 pcall/xpcall 错误被捕获后脚本应继续");

    assert_eq!(report.changed_rows(), 1);
}

#[test]
fn top_level_coroutine_yield_remains_a_script_error() {
    let error = run_project_lua(
        database(),
        request("coroutine.yield('pause')", Arc::new(TestAdapter::default())),
    )
    .expect_err("主程序主动 yield 必须失败");

    assert!(matches!(
        error,
        ProjectLuaRunError::RolledBack(ProjectLuaFailure::Script {
            failure: ProjectLuaScriptFailure::Yielded,
            ..
        })
    ));
}

#[test]
fn running_coroutine_loop_observes_cross_thread_cancellation() {
    let temporary = tempfile::tempdir().expect("应建立临时目录");
    let path = temporary.path().join("coroutine-cancellation.db");
    let setup = Connection::open(&path).expect("应建立数据库");
    setup
        .execute_batch(
            "CREATE TABLE units (id TEXT PRIMARY KEY, translation TEXT);
             INSERT INTO units VALUES ('unit-1', NULL);",
        )
        .expect("应建立测试 schema");
    drop(setup);

    let cancellation = ProjectLuaCancellation::default();
    let worker_cancellation = cancellation.clone();
    let worker_path = path.clone();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let sink = Arc::new(SignalPrintSink {
        sender: Mutex::new(Some(sender)),
    });
    let worker = std::thread::spawn(move || {
        run_project_lua(
            Connection::open(worker_path).expect("worker 应打开数据库"),
            request(
                r#"
ctx.db.execute("UPDATE main.units SET translation = 'must-roll-back'")
print("ready")
local run = coroutine.wrap(function()
  local worker = coroutine.create(function()
    while true do end
  end)
  coroutine.resume(worker)
  while true do end
end)
run()
"#,
                Arc::new(TestAdapter::default()),
            )
            .with_cancellation(worker_cancellation)
            .with_print_sink(sink),
        )
    });
    receiver
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("Lua coroutine 应在超时前开始运行");
    cancellation.cancel();
    let error = worker
        .join()
        .expect("Lua coroutine worker 不应 panic")
        .expect_err("coroutine 内的无限循环应被取消");
    assert_eq!(
        error,
        ProjectLuaRunError::RolledBack(ProjectLuaFailure::Cancelled)
    );
    let translation: Option<String> = Connection::open(path)
        .expect("应重开数据库")
        .query_row(
            "SELECT translation FROM units WHERE id = 'unit-1'",
            [],
            |row| row.get(0),
        )
        .expect("应读取取消后的译文");
    assert_eq!(translation, None);
}

#[test]
fn program_construction_reuses_source_and_argument_allocations() {
    let source = large_lua_comment(256 * 1024);
    let arguments = vec!["first".repeat(32 * 1024), "second".repeat(32 * 1024)];
    let source_pointer = source.as_ptr();
    let arguments_pointer = arguments.as_ptr();
    let program = ProjectLuaProgram::new("shared.lua", source, arguments);

    assert_eq!(program.source().as_ptr(), source_pointer);
    assert_eq!(program.arguments().as_ptr(), arguments_pointer);
}

#[test]
fn chunked_program_fingerprint_is_the_source_file_sha256() {
    let source = large_lua_comment(2 * 64 * 1024 + 17);
    let expected = Sha256Fingerprint::from_bytes(Sha256::digest(&source).into());
    let program = ProjectLuaProgram::new("fingerprint.lua", source, Vec::new());

    let actual = fingerprint_project_lua_program_with_cancellation(
        &program,
        &ProjectLuaCancellation::default(),
    )
    .expect("分块指纹应成功");

    assert_eq!(actual, expected);
}

#[test]
fn utf8_character_split_across_compile_chunks_is_valid() {
    let mut source = large_lua_comment(64 * 1024 - 1);
    source.extend_from_slice("界".as_bytes());
    let program = ProjectLuaProgram::new("utf8-boundary.lua", source, Vec::new());

    compile_project_lua_program(&program).expect("跨分块的 UTF-8 字符应正常编译");
}

#[test]
fn preflight_compilation_observes_cross_thread_cancellation() {
    let program = ProjectLuaProgram::new(
        "large-preflight.lua",
        large_lua_comment(16 * 1024 * 1024),
        Vec::new(),
    );
    let cancellation = ProjectLuaCancellation::default();
    let worker_cancellation = cancellation.clone();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let worker_barrier = Arc::clone(&barrier);
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        worker_barrier.wait();
        sender
            .send(compile_project_lua_program_with_cancellation(
                &program,
                &worker_cancellation,
            ))
            .expect("测试结果接收端不应提前关闭");
    });

    barrier.wait();
    std::thread::sleep(std::time::Duration::from_millis(1));
    cancellation.cancel();
    let result = receiver
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("大脚本预检取消必须在超时前结束");
    worker.join().expect("预检 worker 不应 panic");
    assert_eq!(result, Err(ProjectLuaFailure::Cancelled));
}

#[test]
fn execution_second_compile_observes_cross_thread_cancellation() {
    let program = ProjectLuaProgram::new(
        "large-execution.lua",
        large_lua_comment(16 * 1024 * 1024),
        Vec::new(),
    );
    compile_project_lua_program(&program).expect("预检编译应先成功");

    let cancellation = ProjectLuaCancellation::default();
    let worker_cancellation = cancellation.clone();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let worker_barrier = Arc::clone(&barrier);
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        worker_barrier.wait();
        let result = run_project_lua(
            database(),
            ProjectLuaRunRequest::new(
                ProjectLuaProject::new("test-project", "generic"),
                program,
                Arc::new(TestAdapter::default()),
            )
            .with_cancellation(worker_cancellation),
        );
        sender.send(result).expect("测试结果接收端不应提前关闭");
    });

    barrier.wait();
    std::thread::sleep(std::time::Duration::from_millis(1));
    cancellation.cancel();
    let error = receiver
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("执行期二次编译取消必须在超时前结束")
        .expect_err("执行期二次编译应被取消");
    worker.join().expect("执行 worker 不应 panic");
    assert_eq!(
        error,
        ProjectLuaRunError::NotStarted(ProjectLuaFailure::Cancelled)
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
        ProjectLuaRunError::NotStarted(ProjectLuaFailure::Compile {
            failure: ProjectLuaCompilationFailure::InvalidUtf8,
            ..
        })
    ));
}

#[test]
fn syntax_error_keeps_compiler_line_without_exposing_backend_message() {
    let program = ProjectLuaProgram::new(
        "invalid.lua",
        b"local first = 1\nlocal = 2\n".to_vec(),
        Vec::new(),
    );
    let error = compile_project_lua_program(&program).expect_err("Lua 语法错误必须失败");
    let ProjectLuaFailure::Compile {
        failure: ProjectLuaCompilationFailure::Backend { category, line },
        ..
    } = &error
    else {
        panic!("必须保留类型化 Lua 编译失败");
    };
    assert_eq!(*category, crate::diagnostic::LuaCompilerCategory::Syntax);
    assert_eq!(*line, Some(2));

    let wire =
        serde_json::to_value(error.preflight_diagnostic_report(std::path::Path::new("project.db")))
            .expect("Lua 编译诊断必须可序列化");
    assert_eq!(wire["primary"]["code"], "lua.compilation");
    assert_eq!(wire["primary"]["issue"]["details"]["problem"]["line"], 2);
    assert_eq!(
        wire["primary"]["issue"]["details"]["problem"]["problem"]["category"],
        "syntax"
    );
    assert!(!wire.to_string().contains("near"));
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

    let expected_locator = vec![
        ("id".to_owned(), ProjectLuaValue::Text("unit-1".to_owned())),
        (
            "nested".to_owned(),
            ProjectLuaValue::Array(vec![
                ProjectLuaValue::Text("a".to_owned()),
                ProjectLuaValue::Boolean(true),
            ]),
        ),
        ("ordinal".to_owned(), ProjectLuaValue::Integer(3)),
    ];
    assert_eq!(
        adapter.seen.lock().expect("测试锁不应中毒").as_ref(),
        Some(&(
            ProjectLuaValue::Object(expected_locator),
            ProjectLuaValue::Blob(vec![0, 255]),
        ))
    );
}
