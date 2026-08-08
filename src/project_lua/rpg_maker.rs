//! RPG Maker MV/MZ 项目的可读 Lua 翻译 API。

use std::collections::BTreeSet;
use std::sync::Arc;

use rusqlite::Connection;

use crate::language::LanguageModuleCatalog;
use crate::manual::{
    ManualClearLocatorError, ManualDatabaseError, ManualDetachedTranslation,
    ManualProjectLuaSnapshot, ManualTranslationEntry, ManualTranslationLocator,
    apply_rpg_maker_manual_translations, clear_rpg_maker_manual_translation,
    load_rpg_maker_manual_lua_snapshot, validate_manual_set,
};
use crate::rpg_maker::RpgMakerEngine;
use crate::rpg_maker::location_codec::RpgMakerProjectionCodec;
use crate::rpg_maker::model::TextUnitRole;
use crate::translation::planning_resource::TerminologyEntry;

use super::{
    ProjectLuaCallError, ProjectLuaEngineAdapter, ProjectLuaOutdatedTranslation,
    ProjectLuaTerminologyEntry, ProjectLuaTranslationContext, ProjectLuaTranslationFilter,
    ProjectLuaTranslationRecord, ProjectLuaTranslationStatus, rollback_translation_api_savepoint,
};

pub(crate) fn rpg_maker_project_lua_adapter(
    engine: RpgMakerEngine,
    language_modules: LanguageModuleCatalog,
) -> Arc<dyn ProjectLuaEngineAdapter> {
    Arc::new(RpgMakerProjectLuaAdapter {
        engine,
        language_modules,
    })
}

struct RpgMakerProjectLuaAdapter {
    engine: RpgMakerEngine,
    language_modules: LanguageModuleCatalog,
}

impl ProjectLuaEngineAdapter for RpgMakerProjectLuaAdapter {
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
        filter_records(records, filter, self.engine)
    }

    fn translation_context(
        &self,
        connection: &Connection,
        ids: Vec<String>,
    ) -> Result<Vec<ProjectLuaTranslationContext>, ProjectLuaCallError> {
        let snapshot = self.snapshot(connection)?;
        let mut result = Vec::with_capacity(ids.len());
        for id in ids {
            let requested = snapshot
                .current
                .index
                .get(&id)
                .ok_or_else(|| unknown_unit(self.engine))?;
            let ManualTranslationLocator::RpgMaker { group_location, .. } = &requested.locator
            else {
                return Err(invalid_project(self.engine));
            };
            let group = snapshot
                .current
                .index
                .entries()
                .iter()
                .filter(|entry| {
                    matches!(
                        &entry.locator,
                        ManualTranslationLocator::RpgMaker {
                            group_location: candidate,
                            ..
                        } if candidate == group_location
                    )
                })
                .collect::<Vec<_>>();
            let speaker = group.iter().find_map(|entry| {
                let ManualTranslationLocator::RpgMaker { unit_role, .. } = &entry.locator else {
                    return None;
                };
                matches!(
                    RpgMakerProjectionCodec::decode_role(unit_role),
                    Ok(TextUnitRole::DialogueSpeaker)
                )
                .then(|| {
                    entry
                        .current_translation
                        .as_ref()
                        .unwrap_or(&entry.source)
                        .join("\n")
                })
                .filter(|value| !value.trim().is_empty())
            });
            let translations = group.into_iter().map(record_from_current).collect();
            result.push(ProjectLuaTranslationContext {
                id,
                speaker,
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
        with_savepoint(connection, self.engine, cancellation, || {
            let snapshot = self.snapshot(connection)?;
            if snapshot.current.index.get(&id).is_none() {
                return Err(unknown_unit(self.engine));
            }
            let write = validate_manual_set(&snapshot.current, &id, translation)
                .map_err(|_| invalid_translation(self.engine))?;
            apply_rpg_maker_manual_translations(connection, &[write])
                .map(|changed| changed as u64)
                .map_err(|error| map_manual_error(error, self.engine))
        })
    }

    fn clear_translation(
        &self,
        connection: &Connection,
        id: String,
        cancellation: &super::ProjectLuaCancellation,
    ) -> Result<u64, ProjectLuaCallError> {
        with_savepoint(connection, self.engine, cancellation, || {
            let snapshot = self.snapshot(connection)?;
            let locator = snapshot
                .clear_locator(&id)
                .map_err(|error| map_clear_locator_error(error, self.engine))?;
            clear_rpg_maker_manual_translation(connection, locator)
                .map_err(|error| map_manual_error(error, self.engine))
        })
    }

    fn list_terminology(
        &self,
        connection: &Connection,
    ) -> Result<Vec<ProjectLuaTerminologyEntry>, ProjectLuaCallError> {
        let canonical_json: String = connection
            .query_row(
                "SELECT canonical_json FROM rpg_maker_translation_resource WHERE resource_kind = 'terminology'",
                [],
                |row| row.get(0),
            )
            .map_err(|error| sqlite_error(error, self.engine))?;
        parse_terminology(&canonical_json, self.engine)
    }
}

impl RpgMakerProjectLuaAdapter {
    fn snapshot(
        &self,
        connection: &Connection,
    ) -> Result<ManualProjectLuaSnapshot, ProjectLuaCallError> {
        load_rpg_maker_manual_lua_snapshot(connection, self.engine, &self.language_modules)
            .map_err(|error| map_manual_error(error, self.engine))
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
    engine: RpgMakerEngine,
) -> Result<Vec<ProjectLuaTranslationRecord>, ProjectLuaCallError> {
    let selected = filter.ids.map(|ids| {
        let mut selected = BTreeSet::new();
        for id in ids {
            if !selected.insert(id) {
                return Err(invalid_table(engine));
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
            return Err(unknown_unit(engine));
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

fn map_clear_locator_error(
    error: ManualClearLocatorError,
    engine: RpgMakerEngine,
) -> ProjectLuaCallError {
    match error {
        ManualClearLocatorError::NotFound => unknown_unit(engine),
        ManualClearLocatorError::Ambiguous => invalid_table(engine),
    }
}

fn parse_terminology(
    canonical_json: &str,
    engine: RpgMakerEngine,
) -> Result<Vec<ProjectLuaTerminologyEntry>, ProjectLuaCallError> {
    let entries = serde_json::from_str::<Vec<TerminologyEntry>>(canonical_json)
        .map_err(|_| invalid_project(engine))?;
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
    engine: RpgMakerEngine,
    cancellation: &super::ProjectLuaCancellation,
    operation: impl FnOnce() -> Result<T, ProjectLuaCallError>,
) -> Result<T, ProjectLuaCallError> {
    cancellation.ensure_running()?;
    connection
        .execute_batch("SAVEPOINT att_translation_api")
        .map_err(|error| sqlite_error(error, engine))?;
    match operation() {
        Ok(value) => {
            if let Err(error) = cancellation.ensure_running() {
                return Err(rollback_translation_api_savepoint(
                    connection,
                    lua_engine(engine),
                    error,
                ));
            }
            connection
                .execute_batch("RELEASE att_translation_api")
                .map_err(|error| sqlite_error(error, engine))?;
            Ok(value)
        }
        Err(error) => Err(rollback_translation_api_savepoint(
            connection,
            lua_engine(engine),
            error,
        )),
    }
}

fn map_manual_error(error: ManualDatabaseError, engine: RpgMakerEngine) -> ProjectLuaCallError {
    match error {
        ManualDatabaseError::Cancelled => ProjectLuaCallError::cancelled(),
        ManualDatabaseError::Sqlite(error) => sqlite_error(error, engine),
        ManualDatabaseError::InvalidProject(_) | ManualDatabaseError::Index(_) => {
            invalid_project(engine)
        }
    }
}

fn lua_engine(engine: RpgMakerEngine) -> crate::diagnostic::LuaEngine {
    match engine {
        RpgMakerEngine::Mv => crate::diagnostic::LuaEngine::Mv,
        RpgMakerEngine::Mz => crate::diagnostic::LuaEngine::Mz,
    }
}

fn sqlite_error(error: rusqlite::Error, engine: RpgMakerEngine) -> ProjectLuaCallError {
    ProjectLuaCallError::sqlite(error).with_engine(lua_engine(engine))
}

fn unknown_unit(engine: RpgMakerEngine) -> ProjectLuaCallError {
    ProjectLuaCallError::violation(crate::diagnostic::LuaValueViolation::UnknownUnit)
        .with_engine(lua_engine(engine))
        .with_field("id")
}

fn invalid_translation(engine: RpgMakerEngine) -> ProjectLuaCallError {
    ProjectLuaCallError::violation(crate::diagnostic::LuaValueViolation::InvalidTranslation)
        .with_engine(lua_engine(engine))
        .with_field("translation")
}

fn invalid_table(engine: RpgMakerEngine) -> ProjectLuaCallError {
    ProjectLuaCallError::violation(crate::diagnostic::LuaValueViolation::InvalidTable)
        .with_engine(lua_engine(engine))
}

fn invalid_project(engine: RpgMakerEngine) -> ProjectLuaCallError {
    ProjectLuaCallError::violation(crate::diagnostic::LuaValueViolation::StateMismatch)
        .with_engine(lua_engine(engine))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollback_cleanup_failure_is_not_a_clean_primary_failure() {
        let connection = Connection::open_in_memory().expect("应建立测试数据库");
        let cancellation = super::super::ProjectLuaCancellation::default();

        let failure = with_savepoint(&connection, RpgMakerEngine::Mv, &cancellation, || {
            connection
                .execute_batch("RELEASE att_translation_api")
                .expect("应移除保存点以注入 ROLLBACK TO 失败");
            Err::<(), _>(
                ProjectLuaCallError::cancelled()
                    .with_engine(lua_engine(RpgMakerEngine::Mv))
                    .with_operation(crate::diagnostic::LuaOperation::SetTranslation),
            )
        })
        .expect_err("ROLLBACK TO 失败必须和主错误一并返回");

        assert_eq!(failure.kind(), "cleanup_failed");
        assert!(!failure.is_cancelled());
        assert_eq!(failure.cleanup_failures.len(), 1);
        assert!(matches!(
            &failure.cleanup_failures[0].issue,
            super::super::ProjectLuaCallIssue::Sqlite { .. }
        ));
    }
}
