use std::sync::{Arc, Mutex};

use rusqlite::Connection;

use super::{
    ProjectLuaCallError, ProjectLuaEngine, ProjectLuaEngineAdapter, ProjectLuaFailure,
    ProjectLuaOutdatedTranslation, ProjectLuaProgram, ProjectLuaProject, ProjectLuaRunError,
    ProjectLuaRunRequest, ProjectLuaTerminologyEntry, ProjectLuaTranslationContext,
    ProjectLuaTranslationFilter, ProjectLuaTranslationRecord, ProjectLuaTranslationStatus,
    current_translation_status, run_project_lua,
};

#[test]
fn current_status_uses_current_translation_before_outdated_manual() {
    assert_eq!(
        current_translation_status(true, false, false, true),
        ProjectLuaTranslationStatus::Translated
    );
}

#[test]
fn rejected_candidate_is_unfinished_instead_of_not_needed() {
    assert_eq!(
        current_translation_status(false, true, false, false),
        ProjectLuaTranslationStatus::Unfinished
    );
}

#[derive(Default)]
struct TestAdapter {
    listed: Mutex<Vec<ProjectLuaTranslationFilter>>,
    contexts: Mutex<Vec<Vec<String>>>,
    sets: Mutex<Vec<(String, Vec<String>)>>,
    clears: Mutex<Vec<String>>,
    cancel_and_fail_set: bool,
}

impl ProjectLuaEngineAdapter for TestAdapter {
    fn list_translations(
        &self,
        _connection: &Connection,
        filter: ProjectLuaTranslationFilter,
    ) -> Result<Vec<ProjectLuaTranslationRecord>, ProjectLuaCallError> {
        self.listed.lock().unwrap().push(filter);
        Ok(vec![record("Skills.json:798:name")])
    }

    fn translation_context(
        &self,
        _connection: &Connection,
        ids: Vec<String>,
    ) -> Result<Vec<ProjectLuaTranslationContext>, ProjectLuaCallError> {
        self.contexts.lock().unwrap().push(ids.clone());
        Ok(ids
            .into_iter()
            .map(|id| ProjectLuaTranslationContext {
                id,
                speaker: Some("老师".to_owned()),
                translations: vec![record("Map023.json:event17:page1:dialogue42")],
            })
            .collect())
    }

    fn set_translation(
        &self,
        _connection: &Connection,
        id: String,
        translation: Vec<String>,
        cancellation: &super::ProjectLuaCancellation,
    ) -> Result<u64, ProjectLuaCallError> {
        if self.cancel_and_fail_set {
            cancellation.cancel();
            return Err(ProjectLuaCallError::violation(
                crate::diagnostic::LuaValueViolation::UnknownUnit,
            )
            .with_engine(crate::diagnostic::LuaEngine::Generic)
            .with_field("id"));
        }
        self.sets.lock().unwrap().push((id, translation));
        Ok(1)
    }

    fn clear_translation(
        &self,
        _connection: &Connection,
        id: String,
        _cancellation: &super::ProjectLuaCancellation,
    ) -> Result<u64, ProjectLuaCallError> {
        self.clears.lock().unwrap().push(id);
        Ok(1)
    }

    fn list_terminology(
        &self,
        _connection: &Connection,
    ) -> Result<Vec<ProjectLuaTerminologyEntry>, ProjectLuaCallError> {
        Ok(vec![ProjectLuaTerminologyEntry {
            term: "攻撃".to_owned(),
            translation: "攻击".to_owned(),
        }])
    }
}

#[derive(Clone, Copy)]
enum SavepointFailureMode {
    CommitOutcomeUnknown,
    CleanupFailed,
}

struct SavepointFailureAdapter {
    mode: SavepointFailureMode,
}

impl ProjectLuaEngineAdapter for SavepointFailureAdapter {
    fn list_translations(
        &self,
        _connection: &Connection,
        _filter: ProjectLuaTranslationFilter,
    ) -> Result<Vec<ProjectLuaTranslationRecord>, ProjectLuaCallError> {
        unreachable!("故障注入脚本不读取译文")
    }

    fn translation_context(
        &self,
        _connection: &Connection,
        _ids: Vec<String>,
    ) -> Result<Vec<ProjectLuaTranslationContext>, ProjectLuaCallError> {
        unreachable!("故障注入脚本不读取语境")
    }

    fn set_translation(
        &self,
        connection: &Connection,
        _id: String,
        _translation: Vec<String>,
        cancellation: &super::ProjectLuaCancellation,
    ) -> Result<u64, ProjectLuaCallError> {
        match self.mode {
            SavepointFailureMode::CommitOutcomeUnknown => {
                connection
                    .execute_batch(
                        "SAVEPOINT att_translation_api;
                         UPDATE generic_unit SET value = 'poisoned' WHERE id = 'entry';
                         RELEASE att_translation_api;",
                    )
                    .map_err(ProjectLuaCallError::sqlite)?;
                return Err(super::handle_translation_api_release_failure(
                    connection,
                    crate::diagnostic::LuaEngine::Generic,
                    cancellation,
                    rusqlite::Error::InvalidQuery,
                ));
            }
            SavepointFailureMode::CleanupFailed => {
                connection
                    .authorizer(Some(|context: rusqlite::hooks::AuthContext<'_>| {
                        if matches!(
                            context.action,
                            rusqlite::hooks::AuthAction::Savepoint {
                                operation: rusqlite::hooks::TransactionOperation::Release
                                    | rusqlite::hooks::TransactionOperation::Rollback,
                                savepoint_name: "att_translation_api",
                            }
                        ) {
                            rusqlite::hooks::Authorization::Deny
                        } else {
                            rusqlite::hooks::Authorization::Allow
                        }
                    }))
                    .map_err(ProjectLuaCallError::sqlite)?;
            }
        }
        super::with_translation_api_savepoint(
            connection,
            crate::diagnostic::LuaEngine::Generic,
            cancellation,
            || {
                connection
                    .execute(
                        "UPDATE generic_unit SET value = 'poisoned' WHERE id = 'entry'",
                        [],
                    )
                    .map_err(ProjectLuaCallError::sqlite)?;
                Ok(1)
            },
        )
    }

    fn clear_translation(
        &self,
        _connection: &Connection,
        _id: String,
        _cancellation: &super::ProjectLuaCancellation,
    ) -> Result<u64, ProjectLuaCallError> {
        unreachable!("故障注入脚本不清除译文")
    }

    fn list_terminology(
        &self,
        _connection: &Connection,
    ) -> Result<Vec<ProjectLuaTerminologyEntry>, ProjectLuaCallError> {
        unreachable!("故障注入脚本不读取术语")
    }
}

fn record(id: &str) -> ProjectLuaTranslationRecord {
    ProjectLuaTranslationRecord {
        id: id.to_owned(),
        kind: "fixed".to_owned(),
        source: vec!["Tails Stomp".to_owned()],
        translation: None,
        status: ProjectLuaTranslationStatus::Unfinished,
        origin: None,
        outdated_manual: Some(ProjectLuaOutdatedTranslation {
            id: id.to_owned(),
            kind: "fixed".to_owned(),
            source: vec!["Old source".to_owned()],
            translation: vec!["旧译文".to_owned()],
        }),
    }
}

fn request(source: &str, adapter: Arc<dyn ProjectLuaEngineAdapter>) -> ProjectLuaRunRequest {
    ProjectLuaRunRequest::new(
        ProjectLuaProject::new("demo", ProjectLuaEngine::Generic),
        ProjectLuaProgram::new("script.lua", source.as_bytes(), Vec::new()),
        adapter,
    )
}

fn database(path: &std::path::Path) -> Connection {
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS generic_unit (
                 id TEXT PRIMARY KEY,
                 value TEXT
             );
             INSERT OR IGNORE INTO generic_unit VALUES ('entry', 'original');",
        )
        .unwrap();
    connection
}

#[test]
fn readable_translation_api_uses_arrays_and_batch_context() {
    let adapter = Arc::new(TestAdapter::default());
    let source = r#"
local rows = ctx.translation.list({
  status = "unfinished",
  ids = { "Skills.json:798:name" },
})
assert(#rows == 1)
assert(rows[1].id == "Skills.json:798:name")
assert(rows[1].type == "fixed")
assert(rows[1].source[1] == "Tails Stomp")
assert(rows[1].status == "unfinished")
assert(rows[1].outdated_manual.translation[1] == "旧译文")

local groups = ctx.translation.context({
  "Map023.json:event17:page1:dialogue42",
  "Skills.json:798:name",
})
assert(#groups == 2)
assert(groups[1].speaker == "老师")
assert(groups[1].translations[1].id == "Map023.json:event17:page1:dialogue42")

local terminology = ctx.terminology.list()
assert(terminology[1].term == "攻撃")
assert(terminology[1].translation == "攻击")

ctx.translation.set("Skills.json:798:name", { "尾击" })
ctx.translation.clear("Skills.json:798:name")
"#;
    run_project_lua(
        Connection::open_in_memory().unwrap(),
        request(source, adapter.clone()),
    )
    .unwrap();

    assert_eq!(
        adapter.listed.lock().unwrap().as_slice(),
        &[ProjectLuaTranslationFilter {
            status: Some(ProjectLuaTranslationStatus::Unfinished),
            ids: Some(vec!["Skills.json:798:name".to_owned()]),
        }]
    );
    assert_eq!(
        adapter.contexts.lock().unwrap().as_slice(),
        &[vec![
            "Map023.json:event17:page1:dialogue42".to_owned(),
            "Skills.json:798:name".to_owned(),
        ]]
    );
    assert_eq!(
        adapter.sets.lock().unwrap().as_slice(),
        &[("Skills.json:798:name".to_owned(), vec!["尾击".to_owned()],)]
    );
    assert_eq!(
        adapter.clears.lock().unwrap().as_slice(),
        &["Skills.json:798:name".to_owned()]
    );
}

#[test]
fn translation_set_rejects_scalar_translations_and_non_string_ids() {
    let adapter = Arc::new(TestAdapter::default());
    for source in [
        r#"ctx.translation.set("Skills.json:798:name", "尾击")"#,
        r#"ctx.translation.set({}, { "尾击" })"#,
    ] {
        let error = run_project_lua(
            Connection::open_in_memory().unwrap(),
            request(source, adapter.clone()),
        )
        .expect_err("旧输入必须被拒绝");
        assert!(matches!(error, ProjectLuaRunError::Failed(_)));
    }
    assert!(adapter.sets.lock().unwrap().is_empty());
}

#[test]
fn raw_sql_can_drop_att_tables_and_disable_foreign_keys() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("project.db");
    let source = r#"
ctx.db.execute("PRAGMA foreign_keys = OFF")
ctx.db.execute("DROP TABLE generic_unit")
ctx.db.execute("CREATE TABLE damaged_state (value TEXT)")
ctx.db.execute("INSERT INTO damaged_state VALUES (?1)", { "乱码状态" })
"#;
    run_project_lua(
        database(&path),
        request(source, Arc::new(TestAdapter::default())),
    )
    .unwrap();

    let connection = Connection::open(&path).unwrap();
    let generic_unit_exists: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name = 'generic_unit')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let damaged: String = connection
        .query_row("SELECT value FROM damaged_state", [], |row| row.get(0))
        .unwrap();
    assert!(!generic_unit_exists);
    assert_eq!(damaged, "乱码状态");
}

#[test]
fn explicit_commit_survives_later_script_failure() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("project.db");
    let source = r#"
ctx.db.execute("BEGIN IMMEDIATE")
ctx.db.execute("UPDATE generic_unit SET value = 'committed' WHERE id = 'entry'")
ctx.db.execute("COMMIT")
error("after commit")
"#;
    run_project_lua(
        database(&path),
        request(source, Arc::new(TestAdapter::default())),
    )
    .expect_err("脚本应在提交后失败");

    let value: String = Connection::open(&path)
        .unwrap()
        .query_row(
            "SELECT value FROM generic_unit WHERE id = 'entry'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(value, "committed");
}

#[test]
fn open_transaction_is_rolled_back_when_script_ends() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("project.db");
    let source = r#"
ctx.db.execute("BEGIN IMMEDIATE")
ctx.db.execute("UPDATE generic_unit SET value = 'uncommitted' WHERE id = 'entry'")
"#;
    let error = run_project_lua(
        database(&path),
        request(source, Arc::new(TestAdapter::default())),
    )
    .expect_err("未关闭事务必须报错并回滚");
    assert!(matches!(error, ProjectLuaRunError::RolledBack(_)));

    let value: String = Connection::open(&path)
        .unwrap()
        .query_row(
            "SELECT value FROM generic_unit WHERE id = 'entry'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(value, "original");
}

#[test]
fn attach_detach_and_load_extension_remain_unavailable() {
    let source = r#"
local attached = pcall(ctx.db.execute, "ATTACH DATABASE ':memory:' AS other")
local detached = pcall(ctx.db.execute, "DETACH DATABASE other")
local extension = pcall(ctx.db.query, "SELECT load_extension('missing')")
assert(not attached)
assert(not detached)
assert(not extension)
"#;
    run_project_lua(
        Connection::open_in_memory().unwrap(),
        request(source, Arc::new(TestAdapter::default())),
    )
    .unwrap();
}

#[test]
fn real_host_error_survives_late_cancellation_and_pcall() {
    let adapter = Arc::new(TestAdapter {
        cancel_and_fail_set: true,
        ..TestAdapter::default()
    });
    let source = r#"
local ok, failure = pcall(ctx.translation.set, "missing", { "译文" })
assert(not ok)
assert(failure.kind == "unit_not_found")
error(failure, 0)
"#;

    let error = run_project_lua(
        Connection::open_in_memory().unwrap(),
        request(source, adapter),
    )
    .expect_err("真实 Host 错误不能被同时到达的取消覆盖");

    match error {
        ProjectLuaRunError::Failed(ProjectLuaFailure::Host(error)) => {
            assert_eq!(error.kind(), "unit_not_found");
        }
        other => panic!("应保留 Host 错误，实际为 {other:?}"),
    }
}

#[test]
fn sqlite_interrupt_is_a_typed_cancellation() {
    let failure =
        ProjectLuaFailure::Host(ProjectLuaCallError::sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_INTERRUPT),
            None,
        )))
        .into_typed_cancellation();

    assert_eq!(failure, ProjectLuaFailure::Cancelled);
}

#[test]
fn ordinary_sqlite_error_is_not_a_typed_cancellation() {
    let failure =
        ProjectLuaFailure::Host(ProjectLuaCallError::sqlite(rusqlite::Error::InvalidQuery))
            .into_typed_cancellation();

    assert!(matches!(failure, ProjectLuaFailure::Host(_)));
}

#[test]
fn outermost_release_failure_is_outcome_unknown_even_when_lua_catches_the_call() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("project.db");
    let source = r#"
local ok = pcall(ctx.translation.set, "entry", { "译文" })
assert(not ok)
pcall(ctx.db.execute, "UPDATE generic_unit SET value = 'after-catch' WHERE id = 'entry'")
"#;

    let error = run_project_lua(
        database(&path),
        request(
            source,
            Arc::new(SavepointFailureAdapter {
                mode: SavepointFailureMode::CommitOutcomeUnknown,
            }),
        ),
    )
    .expect_err("最外层 RELEASE 报错必须终止脚本并报告结果未知");

    assert!(
        matches!(
            &error,
            ProjectLuaRunError::SavepointOutcomeUnknown(ProjectLuaFailure::Host(_))
        ),
        "应报告 savepoint 提交结果未知，实际为 {error:?}"
    );
    let value: String = Connection::open(&path)
        .unwrap()
        .query_row(
            "SELECT value FROM generic_unit WHERE id = 'entry'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(value, "poisoned", "结果未知回归必须覆盖实际已经提交的分支");
}

#[test]
fn savepoint_cleanup_failure_cannot_be_caught_and_committed_by_lua() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("project.db");
    let source = r#"
ctx.db.execute("BEGIN IMMEDIATE")
local ok = pcall(ctx.translation.set, "entry", { "译文" })
assert(not ok)
pcall(ctx.db.execute, "COMMIT")
"#;

    let error = run_project_lua(
        database(&path),
        request(
            source,
            Arc::new(SavepointFailureAdapter {
                mode: SavepointFailureMode::CleanupFailed,
            }),
        ),
    )
    .expect_err("savepoint 清理失败必须毒化脚本并回滚外层事务");

    match error {
        ProjectLuaRunError::RolledBack(ProjectLuaFailure::Host(failure)) => {
            assert_eq!(failure.kind(), "cleanup_failed");
        }
        other => panic!("应报告已回滚的 Host cleanup failure，实际为 {other:?}"),
    }
    let value: String = Connection::open(&path)
        .unwrap()
        .query_row(
            "SELECT value FROM generic_unit WHERE id = 'entry'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(value, "original");
}
