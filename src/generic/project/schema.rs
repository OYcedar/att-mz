//! 当前 Generic schema 的唯一 DDL、首次建库及精确结构校验。

use super::error::{
    GenericProjectError, invalid_database, project_safe_identifier, sqlite_operation_error,
};
use super::resources::{
    GenericCompiledTranslationResources, compile_translation_resources_with_cancellation,
    load_translation_resource_row_with_cancellation,
    load_translation_resources_rows_with_cancellation,
};
use super::transaction::run_cancellable_transaction;
use super::{
    LAYOUT_RULES_RESOURCE, append_text_with_cancellation, bytes_equal_with_cancellation,
    clone_sqlite_text_column_with_cancellation, encode_path,
    ensure_generic_operation_not_cancelled,
};
use crate::diagnostic::{GenericProjectDatabaseProblem, SafeIdentifier};
use crate::execution::CooperativeCancellation;
use crate::language::LanguageId;
use crate::project_name::ProjectName;
use crate::runtime::performance::{RunPerformanceCounters, SqliteTransactionScope};
use crate::runtime::sqlite::AttSqliteCancellableConnection;
use crate::translation::layout_rules::LayoutRuleSet;
use rusqlite::{Connection, params};
use std::path::Path;
use std::sync::OnceLock;

const CREATE_INITIAL_SCHEMA_SQL: &str = "CREATE TABLE generic_project (
                 singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                 project_name TEXT NOT NULL CHECK (length(project_name) > 0),
                 source_root BLOB NOT NULL CHECK (length(source_root) > 0 AND length(source_root) % 2 = 0),
                 source_language TEXT NOT NULL CHECK (length(source_language) > 0),
                 target_language TEXT NOT NULL CHECK (length(target_language) > 0),
                 extracted_raw_fingerprint BLOB CHECK (
                     extracted_raw_fingerprint IS NULL OR length(extracted_raw_fingerprint) = 32
                 ),
                 extracted_asset_fingerprint BLOB CHECK (
                     extracted_asset_fingerprint IS NULL OR length(extracted_asset_fingerprint) = 32
                 ),
                 last_profile_id TEXT CHECK (last_profile_id IS NULL OR length(last_profile_id) > 0),
                 CHECK (
                     (extracted_raw_fingerprint IS NULL) =
                     (extracted_asset_fingerprint IS NULL)
                 )
             ) STRICT;
             CREATE TABLE generic_file (
                 relative_path BLOB PRIMARY KEY
                     CHECK (length(relative_path) > 0 AND length(relative_path) % 2 = 0),
                 ordinal INTEGER NOT NULL UNIQUE CHECK (ordinal >= 0)
             ) STRICT;
             CREATE TABLE generic_group (
                 group_id TEXT PRIMARY KEY CHECK (length(CAST(group_id AS BLOB)) > 0),
                 relative_path BLOB NOT NULL REFERENCES generic_file(relative_path) ON DELETE CASCADE,
                 ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
                 kind TEXT NOT NULL CHECK (length(CAST(kind AS BLOB)) > 0),
                 context_fingerprint BLOB NOT NULL CHECK (length(context_fingerprint) = 32),
                 UNIQUE (relative_path, ordinal)
             ) STRICT;
             CREATE TABLE generic_unit (
                 group_id TEXT NOT NULL REFERENCES generic_group(group_id) ON DELETE CASCADE,
                 unit_id TEXT NOT NULL CHECK (length(CAST(unit_id AS BLOB)) > 0),
                 ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
                 source_text TEXT NOT NULL CHECK (
                     instr(source_text, char(13)) = 0 AND instr(source_text, char(0)) = 0
                 ),
                 translation TEXT,
                 translation_state BLOB CHECK (
                     translation_state IS NULL OR length(translation_state) = 32
                 ),
                 PRIMARY KEY (group_id, unit_id),
                 UNIQUE (group_id, ordinal),
                 CHECK (
                     (translation IS NULL AND translation_state IS NULL)
                     OR
                     (translation IS NOT NULL AND length(trim(translation)) > 0
                      AND instr(translation, char(13)) = 0
                      AND instr(translation, char(0)) = 0
                      AND translation_state IS NOT NULL)
                 )
             ) STRICT;
             CREATE TABLE generic_manual_translation (
                 group_id TEXT NOT NULL CHECK (length(CAST(group_id AS BLOB)) > 0),
                 unit_id TEXT NOT NULL CHECK (length(CAST(unit_id AS BLOB)) > 0),
                 readable_id TEXT NOT NULL CHECK (length(readable_id) > 0),
                 source_json TEXT NOT NULL CHECK (
                     json_valid(source_json) AND json_type(source_json) = 'array'
                 ),
                 translation_json TEXT NOT NULL CHECK (
                     json_valid(translation_json)
                     AND json_type(translation_json) = 'array'
                     AND json_array_length(translation_json) > 0
                 ),
                 applicability_fingerprint BLOB NOT NULL CHECK (
                     length(applicability_fingerprint) = 32
                 ),
                 PRIMARY KEY (group_id, unit_id)
             ) STRICT;
             CREATE TABLE generic_rejected_translation (
                 group_id TEXT NOT NULL,
                 unit_id TEXT NOT NULL,
                 readable_id TEXT NOT NULL CHECK (length(readable_id) > 0),
                 origin TEXT NOT NULL CHECK (origin IN ('automatic', 'manual')),
                 source_json TEXT NOT NULL CHECK (
                     json_valid(source_json)
                     AND json_type(source_json) = 'array'
                     AND json_array_length(source_json) > 0
                 ),
                 candidate_json TEXT NOT NULL CHECK (json_valid(candidate_json)),
                 translation_shape TEXT NOT NULL CHECK (translation_shape = 'free'),
                 group_context BLOB NOT NULL CHECK (length(group_context) = 32),
                 violation_json TEXT NOT NULL CHECK (
                     json_valid(violation_json) AND json_type(violation_json) = 'object'
                 ),
                 planning_state BLOB NOT NULL CHECK (length(planning_state) = 32),
                 PRIMARY KEY (group_id, unit_id),
                 FOREIGN KEY (group_id, unit_id)
                     REFERENCES generic_unit(group_id, unit_id) ON DELETE CASCADE
             ) STRICT;
             CREATE TABLE translation_resource (
                 resource_kind TEXT PRIMARY KEY CHECK (
                     resource_kind IN ('terminology', 'placeholder_rules', 'write_back_layout_rules')
                 ),
                 canonical_json TEXT NOT NULL CHECK (length(canonical_json) > 0)
              ) STRICT;
              INSERT INTO translation_resource (resource_kind, canonical_json)
              VALUES ('terminology', '[]'), ('placeholder_rules', '[]'),
                     ('write_back_layout_rules', '[]');";

#[cfg(test)]
pub(crate) fn create_current_generic_schema_for_test(
    connection: &Connection,
) -> Result<(), rusqlite::Error> {
    connection.execute_batch(CREATE_INITIAL_SCHEMA_SQL)
}
const SELECT_GENERIC_ATT_SCHEMA: &str = "SELECT type, name, tbl_name, sql
    FROM main.sqlite_schema
    WHERE sql IS NOT NULL
      AND tbl_name IN (
          'generic_project',
          'generic_file',
          'generic_group',
          'generic_unit',
          'generic_manual_translation',
          'generic_rejected_translation',
          'translation_resource'
      )
    ORDER BY type, name";
pub(super) fn create_initial_schema(
    connection: &mut AttSqliteCancellableConnection,
    project_name: &ProjectName,
    source_root: &Path,
    source_language: &LanguageId,
    target_language: &LanguageId,
    cancellation: &CooperativeCancellation,
    performance: &RunPerformanceCounters,
) -> Result<(), GenericProjectError> {
    run_cancellable_transaction(
        connection,
        cancellation,
        performance,
        SqliteTransactionScope::DatabaseInitialization,
        "开始建立 Generic schema",
        "提交 Generic schema",
        "回滚 Generic schema",
        |transaction| {
            transaction
                .execute_batch(CREATE_INITIAL_SCHEMA_SQL)
                .map_err(|source| GenericProjectError::Sqlite {
                    operation: "建立 Generic schema",
                    source,
                })?;
            transaction
                .execute(
                    "INSERT INTO main.generic_project (
                 singleton, project_name, source_root, source_language, target_language
             ) VALUES (1, ?1, ?2, ?3, ?4)",
                    params![
                        project_name.as_str(),
                        encode_path(source_root),
                        source_language.as_str(),
                        target_language.as_str()
                    ],
                )
                .map_err(|source| GenericProjectError::Sqlite {
                    operation: "写入 Generic 项目事实",
                    source,
                })?;
            Ok(())
        },
    )
}

#[derive(Debug, Eq, PartialEq)]
struct GenericSchemaObject {
    kind: String,
    name: String,
    table_name: String,
    sql: String,
}

fn read_generic_att_schema_with_cancellation(
    connection: &Connection,
    cancellation: &CooperativeCancellation,
) -> Result<Vec<GenericSchemaObject>, GenericProjectError> {
    const OPERATION: &str = "读取当前 Generic schema";

    ensure_generic_operation_not_cancelled(cancellation)?;
    let mut statement = connection
        .prepare(SELECT_GENERIC_ATT_SCHEMA)
        .map_err(|source| sqlite_operation_error(OPERATION, source))?;
    let mut rows = statement
        .query([])
        .map_err(|source| sqlite_operation_error(OPERATION, source))?;
    let mut objects = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|source| sqlite_operation_error(OPERATION, source))?
    {
        ensure_generic_operation_not_cancelled(cancellation)?;
        objects.push(GenericSchemaObject {
            kind: clone_sqlite_text_column_with_cancellation(row, 0, OPERATION, cancellation)?,
            name: clone_sqlite_text_column_with_cancellation(row, 1, OPERATION, cancellation)?,
            table_name: clone_sqlite_text_column_with_cancellation(
                row,
                2,
                OPERATION,
                cancellation,
            )?,
            sql: clone_sqlite_text_column_with_cancellation(row, 3, OPERATION, cancellation)?,
        });
    }
    drop(rows);
    drop(statement);
    ensure_generic_operation_not_cancelled(cancellation)?;
    Ok(objects)
}

fn expected_generic_att_schema() -> &'static [GenericSchemaObject] {
    static EXPECTED: OnceLock<Vec<GenericSchemaObject>> = OnceLock::new();
    EXPECTED.get_or_init(|| {
        let connection =
            Connection::open_in_memory().expect("当前 Generic schema 必须能在内存数据库中建立");
        connection
            .execute_batch(CREATE_INITIAL_SCHEMA_SQL)
            .expect("当前 Generic schema DDL 必须有效");
        read_generic_att_schema_with_cancellation(&connection, &CooperativeCancellation::default())
            .expect("必须能读取刚建立的当前 Generic schema")
    })
}

/// 检查调用方连接中的 ATT 受管对象是否与当前唯一 Generic schema 完全一致。
///
/// 查询只覆盖 ATT 管理的表及附属于这些表的显式 schema 对象；脚本自己的独立表、
/// 索引、视图和触发器不属于本契约。
#[cfg(test)]
pub(crate) fn validate_current_generic_schema(
    connection: &Connection,
) -> Result<(), GenericProjectError> {
    validate_current_generic_schema_with_cancellation(
        connection,
        &CooperativeCancellation::default(),
    )
}

pub(crate) fn validate_current_generic_schema_with_cancellation(
    connection: &Connection,
    cancellation: &CooperativeCancellation,
) -> Result<(), GenericProjectError> {
    let actual = read_generic_att_schema_with_cancellation(connection, cancellation)?;
    let expected = expected_generic_att_schema();
    validate_generic_schema_objects_with_cancellation(&actual, expected, cancellation)
}

fn validate_generic_schema_objects_with_cancellation(
    actual: &[GenericSchemaObject],
    expected: &[GenericSchemaObject],
    cancellation: &CooperativeCancellation,
) -> Result<(), GenericProjectError> {
    let mut missing = Vec::new();
    for expected_object in expected {
        ensure_generic_operation_not_cancelled(cancellation)?;
        let mut found = false;
        for actual_object in actual {
            ensure_generic_operation_not_cancelled(cancellation)?;
            if schema_object_identity_equal_with_cancellation(
                actual_object,
                expected_object,
                cancellation,
            )? {
                found = true;
                break;
            }
        }
        if !found {
            missing.push(schema_object_label_with_cancellation(
                expected_object,
                cancellation,
            )?);
        }
    }

    let mut definition_mismatches = Vec::new();
    for expected_object in expected {
        ensure_generic_operation_not_cancelled(cancellation)?;
        let mut matching = None;
        for actual_object in actual {
            ensure_generic_operation_not_cancelled(cancellation)?;
            if schema_object_identity_equal_with_cancellation(
                actual_object,
                expected_object,
                cancellation,
            )? {
                matching = Some(actual_object);
                break;
            }
        }
        if let Some(actual_object) = matching
            && !schema_object_equal_with_cancellation(actual_object, expected_object, cancellation)?
        {
            definition_mismatches.push(schema_object_label_with_cancellation(
                expected_object,
                cancellation,
            )?);
        }
    }

    let mut unexpected = Vec::new();
    for actual_object in actual {
        ensure_generic_operation_not_cancelled(cancellation)?;
        let mut found = false;
        for expected_object in expected {
            ensure_generic_operation_not_cancelled(cancellation)?;
            if schema_object_identity_equal_with_cancellation(
                actual_object,
                expected_object,
                cancellation,
            )? {
                found = true;
                break;
            }
        }
        if !found {
            unexpected.push(schema_object_label_with_cancellation(
                actual_object,
                cancellation,
            )?);
        }
    }

    if actual.len() == expected.len()
        && missing.is_empty()
        && definition_mismatches.is_empty()
        && unexpected.is_empty()
    {
        return Ok(());
    }
    ensure_generic_operation_not_cancelled(cancellation)?;
    Err(invalid_database(
        GenericProjectDatabaseProblem::SchemaMismatch {
            expected_count: expected.len(),
            actual_count: actual.len(),
            missing,
            definition_mismatches,
            unexpected,
        },
    ))
}

fn schema_object_identity_equal_with_cancellation(
    left: &GenericSchemaObject,
    right: &GenericSchemaObject,
    cancellation: &CooperativeCancellation,
) -> Result<bool, GenericProjectError> {
    Ok(
        bytes_equal_with_cancellation(left.kind.as_bytes(), right.kind.as_bytes(), cancellation)?
            && bytes_equal_with_cancellation(
                left.name.as_bytes(),
                right.name.as_bytes(),
                cancellation,
            )?,
    )
}

fn schema_object_equal_with_cancellation(
    left: &GenericSchemaObject,
    right: &GenericSchemaObject,
    cancellation: &CooperativeCancellation,
) -> Result<bool, GenericProjectError> {
    Ok(
        schema_object_identity_equal_with_cancellation(left, right, cancellation)?
            && bytes_equal_with_cancellation(
                left.table_name.as_bytes(),
                right.table_name.as_bytes(),
                cancellation,
            )?
            && bytes_equal_with_cancellation(
                left.sql.as_bytes(),
                right.sql.as_bytes(),
                cancellation,
            )?,
    )
}

fn schema_object_label_with_cancellation(
    object: &GenericSchemaObject,
    cancellation: &CooperativeCancellation,
) -> Result<SafeIdentifier, GenericProjectError> {
    let mut label = String::new();
    append_text_with_cancellation(&mut label, &object.kind, cancellation)?;
    append_text_with_cancellation(&mut label, "/", cancellation)?;
    append_text_with_cancellation(&mut label, &object.name, cancellation)?;
    ensure_generic_operation_not_cancelled(cancellation)?;
    Ok(project_safe_identifier(label, "schema_object"))
}
pub(super) fn validate_project_database_with_cancellation(
    connection: &Connection,
    cancellation: &CooperativeCancellation,
) -> Result<GenericCompiledTranslationResources, GenericProjectError> {
    ensure_generic_operation_not_cancelled(cancellation)?;
    validate_current_generic_schema_with_cancellation(connection, cancellation)?;
    ensure_generic_operation_not_cancelled(cancellation)?;
    let resource_count: i64 = connection
        .query_row(
            "SELECT count(*) FROM main.translation_resource
             WHERE resource_kind IN (
                 'terminology', 'placeholder_rules', 'write_back_layout_rules'
             )
               AND length(canonical_json) > 0",
            [],
            |row| row.get(0),
        )
        .map_err(|source| sqlite_operation_error("检查 Generic 翻译资源", source))?;
    if resource_count != 3 {
        return Err(invalid_database(
            GenericProjectDatabaseProblem::TranslationResourceCount {
                expected: 3,
                actual: resource_count,
            },
        ));
    }
    ensure_generic_operation_not_cancelled(cancellation)?;
    let layout_rules_json = load_translation_resource_row_with_cancellation(
        connection,
        LAYOUT_RULES_RESOURCE,
        cancellation,
    )?;
    LayoutRuleSet::from_canonical_json(&layout_rules_json)
        .map_err(GenericProjectError::InvalidLayoutRules)?;
    ensure_generic_operation_not_cancelled(cancellation)?;
    let resources = load_translation_resources_rows_with_cancellation(connection, cancellation)?;
    let (terminology_json, placeholder_rules_json) = resources.into_parts();
    let compiled_resources = compile_translation_resources_with_cancellation(
        terminology_json,
        placeholder_rules_json,
        cancellation,
    )?;
    ensure_generic_operation_not_cancelled(cancellation)?;
    let foreign_key_issue = query_optional_first_text_with_cancellation(
        connection,
        "PRAGMA main.foreign_key_check",
        "检查 Generic 外键",
        cancellation,
    )?;
    if let Some(table) = foreign_key_issue {
        return Err(invalid_database(
            GenericProjectDatabaseProblem::ForeignKeyViolation {
                table: project_safe_identifier(table, "unknown_table"),
            },
        ));
    }
    ensure_generic_operation_not_cancelled(cancellation)?;
    let quick_check = query_optional_first_text_with_cancellation(
        connection,
        "PRAGMA main.quick_check",
        "检查 Generic SQLite 完整性",
        cancellation,
    )?
    .ok_or(GenericProjectError::Sqlite {
        operation: "检查 Generic SQLite 完整性",
        source: rusqlite::Error::QueryReturnedNoRows,
    })?;
    if quick_check != "ok" {
        return Err(invalid_database(
            GenericProjectDatabaseProblem::QuickCheckFailed,
        ));
    }
    ensure_generic_operation_not_cancelled(cancellation)?;
    Ok(compiled_resources)
}

fn query_optional_first_text_with_cancellation(
    connection: &Connection,
    query: &'static str,
    operation: &'static str,
    cancellation: &CooperativeCancellation,
) -> Result<Option<String>, GenericProjectError> {
    ensure_generic_operation_not_cancelled(cancellation)?;
    let mut statement = connection
        .prepare(query)
        .map_err(|source| sqlite_operation_error(operation, source))?;
    let mut rows = statement
        .query([])
        .map_err(|source| sqlite_operation_error(operation, source))?;
    let value = match rows
        .next()
        .map_err(|source| sqlite_operation_error(operation, source))?
    {
        Some(row) => Some(clone_sqlite_text_column_with_cancellation(
            row,
            0,
            operation,
            cancellation,
        )?),
        None => None,
    };
    drop(rows);
    drop(statement);
    ensure_generic_operation_not_cancelled(cancellation)?;
    Ok(value)
}
