//! Generic 项目的原子 Lua 适配器。

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, params};

use crate::fingerprint::Sha256Fingerprint;
use crate::generic::{
    GenericPlaceholderService, GenericProject, TranslationOrigin,
    manual_translation_state_for_connection, validate_project_connection,
    validate_translation_placeholders, validated_manual_translation_state_for_connection,
};

use super::{
    ProjectLuaCallError, ProjectLuaEngineAdapter, ProjectLuaSchemaObjectKind, ProjectLuaValue,
};

const GENERIC_ATT_TABLES: &[&str] = &[
    "generic_project",
    "generic_file",
    "generic_group",
    "generic_unit",
    "translation_resource",
];

type InitialTranslationStates = HashMap<(String, String), Option<InitialTranslationState>>;

/// Generic 项目数据库的 typed translation 与最终校验。
#[derive(Debug)]
pub(crate) struct GenericProjectLuaAdapter {
    expected_project: GenericProject,
    initial_translations: Mutex<Option<InitialTranslationStates>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InitialTranslationState {
    origin: TranslationOrigin,
    state: Sha256Fingerprint,
}

impl GenericProjectLuaAdapter {
    pub(crate) fn new(expected_project: GenericProject) -> Self {
        Self {
            expected_project,
            initial_translations: Mutex::new(None),
        }
    }
}

/// 为命令接线建立 Generic Lua 引擎适配器。
pub(crate) fn generic_project_lua_adapter(
    expected_project: GenericProject,
) -> Arc<dyn ProjectLuaEngineAdapter> {
    Arc::new(GenericProjectLuaAdapter::new(expected_project))
}

impl ProjectLuaEngineAdapter for GenericProjectLuaAdapter {
    fn protects_schema_object(
        &self,
        kind: ProjectLuaSchemaObjectKind,
        name: &str,
        table_name: &str,
    ) -> bool {
        match kind {
            ProjectLuaSchemaObjectKind::Table => is_att_table(name),
            ProjectLuaSchemaObjectKind::Index
            | ProjectLuaSchemaObjectKind::View
            | ProjectLuaSchemaObjectKind::Trigger => is_att_table(table_name),
        }
    }

    fn set_translation(
        &self,
        connection: &Connection,
        locator: ProjectLuaValue,
        translation: ProjectLuaValue,
    ) -> Result<u64, ProjectLuaCallError> {
        let (group_id, unit_id) = parse_locator(locator)?;
        let translation = parse_translation(translation)?;
        let state = validated_manual_translation_state_for_connection(
            connection,
            &group_id,
            &unit_id,
            &translation,
        )
        .map_err(generic_error)?;
        let changed = connection
            .execute(
                "UPDATE main.generic_unit
                 SET translation = ?1,
                     translation_origin = 'manual',
                     translation_state = ?2
                 WHERE group_id = ?3 AND unit_id = ?4",
                params![translation, state.as_bytes().as_slice(), group_id, unit_id],
            )
            .map_err(|source| {
                ProjectLuaCallError::new("sqlite", format!("写入 Generic 人工译文失败：{source}"))
            })?;
        if changed != 1 {
            return Err(ProjectLuaCallError::new(
                "unit_not_found",
                "Generic locator 没有命中唯一 Unit",
            ));
        }
        Ok(u64::try_from(changed).expect("受支持平台的 usize 必须能表示为 u64"))
    }

    fn clear_translation(
        &self,
        connection: &Connection,
        locator: ProjectLuaValue,
    ) -> Result<u64, ProjectLuaCallError> {
        let (group_id, unit_id) = parse_locator(locator)?;
        let changed = connection
            .execute(
                "UPDATE main.generic_unit
                 SET translation = NULL,
                     translation_origin = NULL,
                     translation_state = NULL
                 WHERE group_id = ?1 AND unit_id = ?2",
                params![group_id, unit_id],
            )
            .map_err(|source| {
                ProjectLuaCallError::new("sqlite", format!("清除 Generic 人工译文失败：{source}"))
            })?;
        if changed != 1 {
            return Err(ProjectLuaCallError::new(
                "unit_not_found",
                "Generic locator 没有命中唯一 Unit",
            ));
        }
        Ok(u64::try_from(changed).expect("受支持平台的 usize 必须能表示为 u64"))
    }

    fn capture_database_state(
        &self,
        connection: &Connection,
        project: &super::ProjectLuaProject,
    ) -> Result<(), ProjectLuaCallError> {
        if project.engine() != "generic"
            || project.name() != self.expected_project.project_name().as_str()
        {
            return Err(ProjectLuaCallError::new(
                "project_identity",
                "Lua 项目身份与打开的 Generic 项目不一致",
            ));
        }
        let baseline = capture_initial_translation_states(connection)?;
        let mut slot = self
            .initial_translations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_some() {
            return Err(ProjectLuaCallError::new(
                "generic_project",
                "Generic Lua 适配器不能重复执行",
            ));
        }
        *slot = Some(baseline);
        Ok(())
    }

    fn validate_database(
        &self,
        connection: &Connection,
        project: &super::ProjectLuaProject,
    ) -> Result<(), ProjectLuaCallError> {
        if project.engine() != "generic"
            || project.name() != self.expected_project.project_name().as_str()
        {
            return Err(ProjectLuaCallError::new(
                "project_identity",
                "Lua 项目身份与打开的 Generic 项目不一致",
            ));
        }
        validate_project_connection(connection, &self.expected_project).map_err(generic_error)?;
        let baseline = self
            .initial_translations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let baseline = baseline.as_ref().ok_or_else(|| {
            ProjectLuaCallError::new("generic_project", "缺少 Generic Lua 脚本前译文状态")
        })?;
        self.validate_translation_states(connection, baseline)
    }
}

impl GenericProjectLuaAdapter {
    fn validate_translation_states(
        &self,
        connection: &Connection,
        initial_translations: &InitialTranslationStates,
    ) -> Result<(), ProjectLuaCallError> {
        let placeholder_json: String = connection
            .query_row(
                "SELECT canonical_json
                 FROM main.translation_resource
                 WHERE resource_kind = 'placeholder_rules'",
                [],
                |row| row.get(0),
            )
            .map_err(|source| {
                ProjectLuaCallError::new(
                    "sqlite",
                    format!("读取 Generic Placeholder 资源失败：{source}"),
                )
            })?;
        let service = GenericPlaceholderService::default();
        let definitions = service
            .parse_canonical_json(&placeholder_json)
            .map_err(|source| ProjectLuaCallError::new("placeholder", source.to_string()))?;
        let compiled = service
            .compile(definitions)
            .map_err(|source| ProjectLuaCallError::new("placeholder", source.to_string()))?;

        let mut statement = connection
            .prepare(
                "SELECT generic_unit.group_id, generic_unit.unit_id,
                        generic_group.kind, generic_unit.source_text,
                        generic_unit.translation, generic_unit.translation_origin,
                        generic_unit.translation_state
                 FROM main.generic_unit AS generic_unit
                 JOIN main.generic_group AS generic_group USING (group_id)
                 ORDER BY generic_group.relative_path, generic_group.ordinal,
                          generic_unit.ordinal",
            )
            .map_err(|source| {
                ProjectLuaCallError::new("sqlite", format!("准备检查 Generic 译文失败：{source}"))
            })?;
        type TranslationRow = (
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<Vec<u8>>,
        );
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            })
            .map_err(|source| {
                ProjectLuaCallError::new("sqlite", format!("检查 Generic 译文失败：{source}"))
            })?;
        for row in rows {
            let (group_id, unit_id, kind, source_text, translation, origin, state): TranslationRow =
                row.map_err(|source| {
                    ProjectLuaCallError::new("sqlite", format!("读取 Generic 译文失败：{source}"))
                })?;
            let key = (group_id.clone(), unit_id.clone());
            let initial = initial_translations.get(&key).ok_or_else(|| {
                ProjectLuaCallError::new(
                    "translation_state",
                    "Lua 修改了 Generic Unit 身份或新增了未受管 Unit",
                )
            })?;
            let Some(translation) = translation else {
                if origin.is_some() || state.is_some() {
                    return Err(ProjectLuaCallError::new(
                        "translation_state",
                        "Generic 空译文仍带有 origin 或 state",
                    ));
                }
                continue;
            };
            validate_translation_text(&translation)?;
            validate_translation_placeholders(
                &service,
                &compiled,
                &kind,
                &source_text,
                &translation,
            )
            .map_err(|source| ProjectLuaCallError::new("placeholder", source.to_string()))?;
            let (Some(origin), Some(state)) = (origin, state) else {
                return Err(ProjectLuaCallError::new(
                    "translation_state",
                    "Generic 译文缺少 origin 或 state",
                ));
            };
            let origin = match origin.as_str() {
                "automatic" => TranslationOrigin::Automatic,
                "manual" => TranslationOrigin::Manual,
                _ => {
                    return Err(ProjectLuaCallError::new(
                        "translation_state",
                        "Generic 译文 origin 无效",
                    ));
                }
            };
            let state = Sha256Fingerprint::from_slice(&state).map_err(|source| {
                ProjectLuaCallError::new("translation_state", source.to_string())
            })?;
            let state_unchanged = initial
                .as_ref()
                .is_some_and(|initial| initial.origin == origin && initial.state == state);
            if state_unchanged {
                continue;
            }
            if origin != TranslationOrigin::Manual {
                return Err(ProjectLuaCallError::new(
                    "translation_state",
                    "新增或改变状态的 Generic 译文必须通过人工状态校验",
                ));
            }
            let expected = manual_translation_state_for_connection(connection, &group_id, &unit_id)
                .map_err(generic_error)?;
            if state != expected {
                return Err(ProjectLuaCallError::new(
                    "translation_state",
                    "Generic 人工译文状态与当前项目事实不一致",
                ));
            }
        }
        Ok(())
    }
}

fn capture_initial_translation_states(
    connection: &Connection,
) -> Result<InitialTranslationStates, ProjectLuaCallError> {
    let mut statement = connection
        .prepare(
            "SELECT group_id, unit_id, translation,
                    translation_origin, translation_state
             FROM main.generic_unit",
        )
        .map_err(|source| {
            ProjectLuaCallError::new("sqlite", format!("准备捕获 Generic 译文状态失败：{source}"))
        })?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<Vec<u8>>>(4)?,
            ))
        })
        .map_err(|source| {
            ProjectLuaCallError::new("sqlite", format!("捕获 Generic 译文状态失败：{source}"))
        })?;
    let mut states = HashMap::new();
    for row in rows {
        let (group_id, unit_id, translation, origin, state) = row.map_err(|source| {
            ProjectLuaCallError::new("sqlite", format!("读取 Generic 译文状态失败：{source}"))
        })?;
        let state = match (translation, origin, state) {
            (None, None, None) => None,
            (Some(_), Some(origin), Some(state)) => {
                let origin = match origin.as_str() {
                    "automatic" => TranslationOrigin::Automatic,
                    "manual" => TranslationOrigin::Manual,
                    _ => {
                        return Err(ProjectLuaCallError::new(
                            "translation_state",
                            "Generic 脚本前译文 origin 无效",
                        ));
                    }
                };
                let state = Sha256Fingerprint::from_slice(&state).map_err(|source| {
                    ProjectLuaCallError::new("translation_state", source.to_string())
                })?;
                Some(InitialTranslationState { origin, state })
            }
            _ => {
                return Err(ProjectLuaCallError::new(
                    "translation_state",
                    "Generic 脚本前译文状态不完整",
                ));
            }
        };
        states.insert((group_id, unit_id), state);
    }
    Ok(states)
}

fn is_att_table(name: &str) -> bool {
    GENERIC_ATT_TABLES
        .iter()
        .any(|table| table.eq_ignore_ascii_case(name))
}

fn parse_locator(locator: ProjectLuaValue) -> Result<(String, String), ProjectLuaCallError> {
    let ProjectLuaValue::Object(mut fields) = locator else {
        return Err(ProjectLuaCallError::new(
            "invalid_locator",
            "Generic locator 必须是只含 group_id 与 unit_id 的 table",
        ));
    };
    if fields.len() != 2 || !fields.contains_key("group_id") || !fields.contains_key("unit_id") {
        return Err(ProjectLuaCallError::new(
            "invalid_locator",
            "Generic locator 必须且只能包含 group_id 与 unit_id",
        ));
    }
    let group_id = take_nonempty_text(&mut fields, "group_id")?;
    let unit_id = take_nonempty_text(&mut fields, "unit_id")?;
    Ok((group_id, unit_id))
}

fn take_nonempty_text(
    fields: &mut BTreeMap<String, ProjectLuaValue>,
    field: &'static str,
) -> Result<String, ProjectLuaCallError> {
    let Some(ProjectLuaValue::Text(value)) = fields.remove(field) else {
        return Err(ProjectLuaCallError::new(
            "invalid_locator",
            format!("Generic locator.{field} 必须是字符串"),
        ));
    };
    if value.is_empty() {
        return Err(ProjectLuaCallError::new(
            "invalid_locator",
            format!("Generic locator.{field} 不能为空"),
        ));
    }
    Ok(value)
}

fn parse_translation(value: ProjectLuaValue) -> Result<String, ProjectLuaCallError> {
    let ProjectLuaValue::Text(value) = value else {
        return Err(ProjectLuaCallError::new(
            "invalid_translation",
            "Generic translation 必须是 UTF-8 字符串",
        ));
    };
    validate_translation_text(&value)?;
    Ok(value)
}

fn validate_translation_text(value: &str) -> Result<(), ProjectLuaCallError> {
    if value.chars().all(char::is_whitespace) {
        return Err(ProjectLuaCallError::new(
            "invalid_translation",
            "Generic translation 不能为空白",
        ));
    }
    if value.contains('\r') {
        return Err(ProjectLuaCallError::new(
            "invalid_translation",
            "Generic translation 不能包含 CR（U+000D）",
        ));
    }
    if value.contains('\0') {
        return Err(ProjectLuaCallError::new(
            "invalid_translation",
            "Generic translation 不能包含 NUL（U+0000）",
        ));
    }
    Ok(())
}

fn generic_error(error: crate::generic::GenericProjectError) -> ProjectLuaCallError {
    ProjectLuaCallError::new("generic_project", error.to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rusqlite::{OpenFlags, OptionalExtension};
    use tempfile::tempdir;

    use crate::generic::{
        GenericInitRequest, GenericProjectStore, manual_translation_state_for_connection,
    };
    use crate::language::LanguageId;

    use super::*;
    use crate::project_lua::{
        ProjectLuaFailure, ProjectLuaProgram, ProjectLuaProject, ProjectLuaRunError,
        ProjectLuaRunRequest, run_project_lua,
    };

    fn project() -> (tempfile::TempDir, GenericProjectStore, std::path::PathBuf) {
        let temporary = tempdir().expect("应建立临时目录");
        let source_root = temporary.path().join("source");
        fs::create_dir(&source_root).expect("应建立来源目录");
        fs::write(
            source_root.join("text.jsonl"),
            "{\"id\":\"opening\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"body\",\"text\":\"こんにちは {name}\"}]}\n",
        )
        .expect("应写入 Generic JSONL");
        let workspace = temporary.path().join("project");
        let (store, project) = GenericProjectStore::initialize(GenericInitRequest {
            project_name: "game".parse().expect("项目名应合法"),
            workspace_root: workspace,
            source_root: Some(source_root),
            source_language: Some(LanguageId::parse("ja").expect("来源语言应合法")),
            target_language: Some(LanguageId::parse("zh-Hans").expect("目标语言应合法")),
        })
        .expect("应初始化 Generic 项目");
        store.extract().expect("应完成 Generic Extract");
        let snapshot = store.load_snapshot().expect("应读取 Generic 快照");
        store
            .apply_translation_resources(
                snapshot
                    .project()
                    .extracted_raw_fingerprint()
                    .expect("Extract 应保存指纹"),
                "[]",
                r#"[{"pattern":"\\{[^}]+\\}"}]"#,
                &[],
            )
            .expect("应保存 Placeholder 资源");
        (temporary, store, project.database_path().to_path_buf())
    }

    fn run(
        database_path: &std::path::Path,
        source: &str,
    ) -> Result<super::super::ProjectLuaRunReport, ProjectLuaRunError> {
        let expected_project = GenericProjectStore::for_workspace(
            database_path
                .parent()
                .expect("Generic 数据库应位于项目目录")
                .to_path_buf(),
        )
        .open()
        .expect("应在 Lua 前打开 Generic 项目");
        let connection =
            Connection::open_with_flags(database_path, OpenFlags::SQLITE_OPEN_READ_WRITE)
                .expect("应打开已有 Generic 数据库");
        run_project_lua(
            connection,
            ProjectLuaRunRequest::new(
                ProjectLuaProject::new("game", "generic"),
                ProjectLuaProgram::new("generic.lua", source.as_bytes(), Vec::new()),
                generic_project_lua_adapter(expected_project),
            ),
        )
    }

    #[test]
    fn typed_set_and_clear_change_exactly_one_generic_unit() {
        let (_temporary, _store, database_path) = project();
        let report = run(
            &database_path,
            r#"ctx.translation.set(
                 {group_id = "opening", unit_id = "body"},
                 "你好 {name}"
               )"#,
        )
        .expect("typed set 应成功");
        assert_eq!(report.translation_calls(), 1);

        let connection = Connection::open(&database_path).expect("应重开数据库");
        let expected_state =
            manual_translation_state_for_connection(&connection, "opening", "body")
                .expect("应重建人工状态");
        let (translation, origin, state): (String, String, Vec<u8>) = connection
            .query_row(
                "SELECT translation, translation_origin, translation_state
                 FROM generic_unit WHERE group_id = 'opening' AND unit_id = 'body'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("应读取人工译文");
        assert_eq!(translation, "你好 {name}");
        assert_eq!(origin, "manual");
        assert_eq!(state, expected_state.as_bytes());
        drop(connection);

        run(
            &database_path,
            r#"ctx.translation.clear({group_id = "opening", unit_id = "body"})"#,
        )
        .expect("typed clear 应成功");
        let cleared: Option<String> = Connection::open(database_path)
            .expect("应重开数据库")
            .query_row(
                "SELECT translation FROM generic_unit
                 WHERE group_id = 'opening' AND unit_id = 'body'",
                [],
                |row| row.get(0),
            )
            .expect("应读取清理结果");
        assert_eq!(cleared, None);
    }

    #[test]
    fn locator_preserves_nonempty_whitespace_identities() {
        let locator = ProjectLuaValue::Object(BTreeMap::from([
            ("group_id".to_owned(), ProjectLuaValue::Text(" ".to_owned())),
            ("unit_id".to_owned(), ProjectLuaValue::Text("\t".to_owned())),
        ]));

        assert_eq!(
            parse_locator(locator).expect("纯空白但非空的 Generic ID 应按原值合法"),
            (" ".to_owned(), "\t".to_owned())
        );
    }

    #[test]
    fn invalid_locator_and_translation_roll_back_without_changing_unit() {
        let (_temporary, _store, database_path) = project();
        for source in [
            r#"ctx.translation.set({group_id = "opening"}, "译文")"#,
            r#"ctx.translation.set(
                 {group_id = "opening", unit_id = "missing"},
                 "译文"
               )"#,
            r#"ctx.translation.set(
                 {group_id = "opening", unit_id = "body"},
                 "   "
               )"#,
            r#"ctx.translation.set(
                 {group_id = "opening", unit_id = "body"},
                 {"数组"}
               )"#,
        ] {
            assert!(matches!(
                run(&database_path, source),
                Err(ProjectLuaRunError::RolledBack(
                    ProjectLuaFailure::Host { .. }
                ))
            ));
        }
        let translation: Option<String> = Connection::open(database_path)
            .expect("应重开数据库")
            .query_row(
                "SELECT translation FROM generic_unit
                 WHERE group_id = 'opening' AND unit_id = 'body'",
                [],
                |row| row.get(0),
            )
            .expect("应读取未修改状态");
        assert_eq!(translation, None);
    }

    #[test]
    fn direct_sql_keeps_manual_state_and_invalid_resource_is_rejected_at_commit() {
        let (_temporary, _store, database_path) = project();
        run(
            &database_path,
            r#"ctx.translation.set(
                 {group_id = "opening", unit_id = "body"},
                 "你好 {name}"
               )"#,
        )
        .expect("typed set 应成功");
        let state_before: Vec<u8> = Connection::open(&database_path)
            .expect("应重开数据库")
            .query_row(
                "SELECT translation_state FROM generic_unit
                 WHERE group_id = 'opening' AND unit_id = 'body'",
                [],
                |row| row.get(0),
            )
            .expect("应读取人工状态");

        run(
            &database_path,
            r#"ctx.db.execute(
                 "UPDATE generic_unit SET translation = '人工修订 {name}' " ..
                 "WHERE group_id = 'opening' AND unit_id = 'body'"
               )"#,
        )
        .expect("直接修订已有译文应保留状态");
        let connection = Connection::open(&database_path).expect("应重开数据库");
        let state_after: Vec<u8> = connection
            .query_row(
                "SELECT translation_state FROM generic_unit
                 WHERE group_id = 'opening' AND unit_id = 'body'",
                [],
                |row| row.get(0),
            )
            .expect("应读取修订后状态");
        assert_eq!(state_after, state_before);
        drop(connection);

        let error = run(
            &database_path,
            r#"ctx.db.execute(
                 "UPDATE translation_resource SET canonical_json = '{}' " ..
                 "WHERE resource_kind = 'placeholder_rules'"
               )"#,
        )
        .expect_err("无效资源不能提交");
        assert!(matches!(
            error,
            ProjectLuaRunError::RolledBack(ProjectLuaFailure::Host {
                operation: "translation.validate",
                ..
            })
        ));
        let resource: String = Connection::open(database_path)
            .expect("应重开数据库")
            .query_row(
                "SELECT canonical_json FROM translation_resource
                 WHERE resource_kind = 'placeholder_rules'",
                [],
                |row| row.get(0),
            )
            .optional()
            .expect("应读取资源")
            .expect("资源应存在");
        assert_ne!(resource, "{}");
    }

    #[test]
    fn direct_sql_cannot_forge_new_translation_state() {
        let (_temporary, _store, database_path) = project();
        for (origin, state) in [("automatic", "zeroblob(32)"), ("manual", "zeroblob(32)")] {
            let source = format!(
                "ctx.db.execute(
                    [=[UPDATE main.generic_unit
                     SET translation = '你好 {{name}}',
                         translation_origin = '{origin}',
                         translation_state = {state}
                     WHERE group_id = 'opening' AND unit_id = 'body']=]
                 )"
            );
            let result = run(&database_path, &source);
            assert!(
                matches!(
                    result,
                    Err(ProjectLuaRunError::RolledBack(ProjectLuaFailure::Host {
                        operation: "translation.validate",
                        ..
                    }))
                ),
                "伪造 {origin} 状态的实际结果：{result:?}"
            );
        }

        let translation: Option<String> = Connection::open(&database_path)
            .expect("应重开数据库")
            .query_row(
                "SELECT translation FROM main.generic_unit
                 WHERE group_id = 'opening' AND unit_id = 'body'",
                [],
                |row| row.get(0),
            )
            .expect("应读取回滚后的译文");
        assert_eq!(translation, None);
    }

    #[test]
    fn direct_sql_cannot_change_generic_project_or_extract_snapshot_facts() {
        let (_temporary, _store, database_path) = project();
        for statement in [
            "UPDATE main.generic_project SET project_name = 'other' WHERE singleton = 1",
            "UPDATE main.generic_project SET source_language = 'en' WHERE singleton = 1",
            "UPDATE main.generic_project SET extracted_asset_fingerprint = zeroblob(32)
             WHERE singleton = 1",
            "UPDATE main.generic_group SET context_fingerprint = zeroblob(32)
             WHERE group_id = 'opening'",
            "UPDATE main.generic_unit SET source_text = 'さようなら {name}'
             WHERE group_id = 'opening' AND unit_id = 'body'",
        ] {
            let source = format!("ctx.db.execute({statement:?})");
            assert!(matches!(
                run(&database_path, &source),
                Err(ProjectLuaRunError::RolledBack(ProjectLuaFailure::Host {
                    operation: "translation.validate",
                    ..
                }))
            ));
        }

        let connection = Connection::open(&database_path).expect("应重开数据库");
        let (project_name, source_language): (String, String) = connection
            .query_row(
                "SELECT project_name, source_language
                 FROM main.generic_project WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("应读取项目事实");
        let source_text: String = connection
            .query_row(
                "SELECT source_text FROM main.generic_unit
                 WHERE group_id = 'opening' AND unit_id = 'body'",
                [],
                |row| row.get(0),
            )
            .expect("应读取来源正文");
        assert_eq!(project_name, "game");
        assert_eq!(source_language, "ja");
        assert_eq!(source_text, "こんにちは {name}");
    }

    #[test]
    fn generic_att_schema_cannot_be_removed() {
        let (_temporary, _store, database_path) = project();
        run(
            &database_path,
            r#"
local ok = pcall(ctx.db.execute, "DROP TABLE generic_unit")
assert(not ok)
ctx.db.execute("CREATE TABLE lua_private (value TEXT)")
"#,
        )
        .expect("受管 schema 拒绝可捕获，私有表应提交");
        run(
            &database_path,
            r#"
local ok = pcall(ctx.db.execute, "DROP TABLE GENERIC_UNIT")
assert(not ok)
"#,
        )
        .expect("受管表名保护应忽略 ASCII 大小写");
        let private_exists: i64 = Connection::open(database_path)
            .expect("应重开数据库")
            .query_row(
                "SELECT count(*) FROM sqlite_schema
                 WHERE type = 'table' AND name = 'lua_private'",
                [],
                |row| row.get(0),
            )
            .expect("应检查私有表");
        assert_eq!(private_exists, 1);
    }
}
