//! Generic 项目的可读 Lua 翻译 API。

use std::collections::BTreeSet;
use std::sync::Arc;

use rusqlite::Connection;

use crate::language::LanguageModuleCatalog;
use crate::manual::{
    ManualClearLocatorError, ManualDatabaseError, ManualDetachedTranslation,
    ManualProjectLuaSnapshot, ManualTranslationEntry, ManualTranslationLocator,
    apply_generic_manual_translations, clear_generic_manual_translation,
    load_generic_manual_lua_snapshot, validate_manual_set,
};
use crate::translation::planning_resource::TerminologyEntry;

use super::{
    ProjectLuaCallError, ProjectLuaEngineAdapter, ProjectLuaOutdatedTranslation,
    ProjectLuaTerminologyEntry, ProjectLuaTranslationContext, ProjectLuaTranslationFilter,
    ProjectLuaTranslationRecord, ProjectLuaTranslationStatus,
};

pub(crate) fn generic_project_lua_adapter_for_name(
    _project_name: String,
    language_modules: LanguageModuleCatalog,
) -> Arc<dyn ProjectLuaEngineAdapter> {
    Arc::new(GenericProjectLuaAdapter { language_modules })
}

struct GenericProjectLuaAdapter {
    language_modules: LanguageModuleCatalog,
}

impl ProjectLuaEngineAdapter for GenericProjectLuaAdapter {
    fn list_translations(
        &self,
        connection: &Connection,
        filter: ProjectLuaTranslationFilter,
    ) -> Result<Vec<ProjectLuaTranslationRecord>, ProjectLuaCallError> {
        let snapshot = self.snapshot(connection)?;
        let mut records = snapshot
            .current
            .index
            .entries()
            .iter()
            .map(record_from_current)
            .collect::<Vec<_>>();
        records.extend(snapshot.detached.iter().map(record_from_detached));
        filter_records(records, filter)
    }

    fn translation_context(
        &self,
        connection: &Connection,
        ids: Vec<String>,
    ) -> Result<Vec<ProjectLuaTranslationContext>, ProjectLuaCallError> {
        let snapshot = self.snapshot(connection)?;
        let mut result = Vec::with_capacity(ids.len());
        for id in ids {
            let requested = snapshot.current.index.get(&id).ok_or_else(unknown_unit)?;
            let ManualTranslationLocator::Generic { group_id, .. } = &requested.locator else {
                return Err(invalid_project());
            };
            let translations = snapshot
                .current
                .index
                .entries()
                .iter()
                .filter(|entry| {
                    matches!(
                        &entry.locator,
                        ManualTranslationLocator::Generic {
                            group_id: candidate,
                            ..
                        } if candidate == group_id
                    )
                })
                .map(record_from_current)
                .collect();
            result.push(ProjectLuaTranslationContext {
                id,
                speaker: None,
                translations,
            });
        }
        Ok(result)
    }

    fn set_translation(
        &self,
        connection: &Connection,
        id: String,
        translation: Vec<String>,
        cancellation: &super::ProjectLuaCancellation,
    ) -> Result<u64, ProjectLuaCallError> {
        with_savepoint(connection, cancellation, || {
            let snapshot = self.snapshot(connection)?;
            if snapshot.current.index.get(&id).is_none() {
                return Err(unknown_unit());
            }
            let write = validate_manual_set(&snapshot.current, &id, translation)
                .map_err(|_| invalid_translation())?;
            apply_generic_manual_translations(connection, &[write])
                .map(|changed| changed as u64)
                .map_err(map_manual_error)
        })
    }

    fn clear_translation(
        &self,
        connection: &Connection,
        id: String,
        cancellation: &super::ProjectLuaCancellation,
    ) -> Result<u64, ProjectLuaCallError> {
        with_savepoint(connection, cancellation, || {
            let snapshot = self.snapshot(connection)?;
            let locator = snapshot
                .clear_locator(&id)
                .map_err(map_clear_locator_error)?;
            clear_generic_manual_translation(connection, locator).map_err(map_manual_error)
        })
    }

    fn list_terminology(
        &self,
        connection: &Connection,
    ) -> Result<Vec<ProjectLuaTerminologyEntry>, ProjectLuaCallError> {
        let canonical_json: String = connection
            .query_row(
                "SELECT canonical_json FROM translation_resource WHERE resource_kind = 'terminology'",
                [],
                |row| row.get(0),
            )
            .map_err(sqlite_error)?;
        parse_terminology(&canonical_json)
    }
}

impl GenericProjectLuaAdapter {
    fn snapshot(
        &self,
        connection: &Connection,
    ) -> Result<ManualProjectLuaSnapshot, ProjectLuaCallError> {
        load_generic_manual_lua_snapshot(connection, &self.language_modules)
            .map_err(map_manual_error)
    }
}

fn record_from_current(entry: &ManualTranslationEntry) -> ProjectLuaTranslationRecord {
    let status = if entry.outdated_manual.is_some() {
        ProjectLuaTranslationStatus::Outdated
    } else if entry.current_translation.is_some() {
        ProjectLuaTranslationStatus::Translated
    } else if entry.needs_translation {
        ProjectLuaTranslationStatus::Unfinished
    } else {
        ProjectLuaTranslationStatus::NotNeeded
    };
    ProjectLuaTranslationRecord {
        id: entry.id.clone(),
        kind: entry.kind.as_str().to_owned(),
        source: entry.source.clone(),
        translation: entry.current_translation.clone(),
        status,
        origin: entry.origin.map(|origin| origin.as_str().to_owned()),
        outdated_manual: entry.outdated_manual.as_ref().map(|manual| {
            ProjectLuaOutdatedTranslation {
                id: manual.id.clone(),
                kind: manual.kind.as_str().to_owned(),
                source: manual.source.clone(),
                translation: manual.translation.clone(),
            }
        }),
    }
}

fn record_from_detached(entry: &ManualDetachedTranslation) -> ProjectLuaTranslationRecord {
    let manual = &entry.snapshot;
    ProjectLuaTranslationRecord {
        id: manual.id.clone(),
        kind: manual.kind.as_str().to_owned(),
        source: manual.source.clone(),
        translation: None,
        status: ProjectLuaTranslationStatus::Outdated,
        origin: None,
        outdated_manual: Some(ProjectLuaOutdatedTranslation {
            id: manual.id.clone(),
            kind: manual.kind.as_str().to_owned(),
            source: manual.source.clone(),
            translation: manual.translation.clone(),
        }),
    }
}

fn filter_records(
    records: Vec<ProjectLuaTranslationRecord>,
    filter: ProjectLuaTranslationFilter,
) -> Result<Vec<ProjectLuaTranslationRecord>, ProjectLuaCallError> {
    let selected = filter.ids.map(|ids| {
        let mut selected = BTreeSet::new();
        for id in ids {
            if !selected.insert(id) {
                return Err(invalid_table());
            }
        }
        Ok(selected)
    });
    let selected = match selected {
        Some(selected) => Some(selected?),
        None => None,
    };
    if let Some(selected) = selected.as_ref() {
        let known = records
            .iter()
            .map(|record| record.id.as_str())
            .collect::<BTreeSet<_>>();
        if selected.iter().any(|id| !known.contains(id.as_str())) {
            return Err(unknown_unit());
        }
    }
    Ok(records
        .into_iter()
        .filter(|record| {
            filter.status.is_none_or(|status| record.status == status)
                && selected.as_ref().is_none_or(|ids| ids.contains(&record.id))
        })
        .collect())
}

fn map_clear_locator_error(error: ManualClearLocatorError) -> ProjectLuaCallError {
    match error {
        ManualClearLocatorError::NotFound => unknown_unit(),
        ManualClearLocatorError::Ambiguous => invalid_table(),
    }
}

fn parse_terminology(
    canonical_json: &str,
) -> Result<Vec<ProjectLuaTerminologyEntry>, ProjectLuaCallError> {
    let entries = serde_json::from_str::<Vec<TerminologyEntry>>(canonical_json)
        .map_err(|_| invalid_project())?;
    Ok(entries
        .into_iter()
        .map(|entry| ProjectLuaTerminologyEntry {
            term: entry.term().to_owned(),
            translation: entry.translation().to_owned(),
        })
        .collect())
}

fn with_savepoint<T>(
    connection: &Connection,
    cancellation: &super::ProjectLuaCancellation,
    operation: impl FnOnce() -> Result<T, ProjectLuaCallError>,
) -> Result<T, ProjectLuaCallError> {
    cancellation.ensure_running()?;
    connection
        .execute_batch("SAVEPOINT att_translation_api")
        .map_err(sqlite_error)?;
    match operation() {
        Ok(value) => {
            if let Err(error) = cancellation.ensure_running() {
                let _ = connection
                    .execute_batch("ROLLBACK TO att_translation_api; RELEASE att_translation_api");
                return Err(error);
            }
            connection
                .execute_batch("RELEASE att_translation_api")
                .map_err(sqlite_error)?;
            Ok(value)
        }
        Err(error) => {
            let _ = connection
                .execute_batch("ROLLBACK TO att_translation_api; RELEASE att_translation_api");
            Err(error)
        }
    }
}

fn map_manual_error(error: ManualDatabaseError) -> ProjectLuaCallError {
    match error {
        ManualDatabaseError::Cancelled => ProjectLuaCallError::cancelled(),
        ManualDatabaseError::Sqlite(error) => sqlite_error(error),
        ManualDatabaseError::InvalidProject(_) | ManualDatabaseError::Index(_) => invalid_project(),
    }
}

fn sqlite_error(error: rusqlite::Error) -> ProjectLuaCallError {
    ProjectLuaCallError::sqlite(error).with_engine(crate::diagnostic::LuaEngine::Generic)
}

fn unknown_unit() -> ProjectLuaCallError {
    ProjectLuaCallError::violation(crate::diagnostic::LuaValueViolation::UnknownUnit)
        .with_engine(crate::diagnostic::LuaEngine::Generic)
        .with_field("id")
}

fn invalid_translation() -> ProjectLuaCallError {
    ProjectLuaCallError::violation(crate::diagnostic::LuaValueViolation::InvalidTranslation)
        .with_engine(crate::diagnostic::LuaEngine::Generic)
        .with_field("translation")
}

fn invalid_table() -> ProjectLuaCallError {
    ProjectLuaCallError::violation(crate::diagnostic::LuaValueViolation::InvalidTable)
        .with_engine(crate::diagnostic::LuaEngine::Generic)
}

fn invalid_project() -> ProjectLuaCallError {
    ProjectLuaCallError::violation(crate::diagnostic::LuaValueViolation::StateMismatch)
        .with_engine(crate::diagnostic::LuaEngine::Generic)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_before_savepoint_release_rolls_back_the_translation_change() {
        let connection = Connection::open_in_memory().expect("应建立测试数据库");
        connection
            .execute_batch("CREATE TABLE changed(value INTEGER NOT NULL);")
            .expect("应建立测试表");
        let cancellation = super::super::ProjectLuaCancellation::default();
        let operation_cancellation = cancellation.clone();

        let failure = with_savepoint(&connection, &cancellation, || {
            connection
                .execute("INSERT INTO changed(value) VALUES (1)", [])
                .map_err(sqlite_error)?;
            operation_cancellation.cancel();
            Ok(())
        })
        .expect_err("取消到达后不得释放保存点");

        assert_eq!(failure.kind(), "cancelled");
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM changed", [], |row| row
                    .get::<_, i64>(0))
                .expect("应读取回滚结果"),
            0
        );
    }
}
