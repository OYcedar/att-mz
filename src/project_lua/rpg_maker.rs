//! RPG Maker 项目的原子 Lua 适配器。

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::io::{self, BufReader, Read, Write};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use rusqlite::types::{Type, ValueRef};
use rusqlite::{Connection, Row, params};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};
use crate::language::{LanguageId, LanguagePair};
use crate::project_name::ProjectName;
use crate::rpg_maker::RpgMakerEngine;
use crate::rpg_maker::asset::{RpgMakerAssetOwner, RpgMakerTextSnapshotFingerprintBuilder};
use crate::rpg_maker::location_codec::{RpgMakerLocationCodec, RpgMakerProjectionCodec};
use crate::rpg_maker::model::{
    MutationResourceAccess, TextProjectionRecipe, TextUnitContent, TextUnitContentView,
    TextUnitRole, validate_text_unit_content_structure,
};
use crate::rpg_maker::mutation_claim_summary::{
    EncodedMutationClaim, collision_summary, sort_logical_claims,
};
use crate::rpg_maker::project_database::{
    CurrentAttSchemaValidationError, SELECT_RUN_PLAN_SINGLETONS, decode_project_run_plans,
    max_fullwidth_chars_from_rusqlite_value, validate_current_att_schema_with_cancellation,
    validate_mv_dialogue_definition_canonical_json,
};
use crate::rpg_maker::semantic_order::RpgMakerSemanticOrderKey;
use crate::rpg_maker::text::TextGroupKind;
use crate::rpg_maker::translate::pipeline::{
    AppliedPlaceholder, GroupContextFingerprint, PlaceholderRuleOrigin, PlaceholderSegment,
    TerminologyDependency, TranslationUnitIdentity,
};
use crate::rpg_maker::translate::placeholder::{
    CompiledPlaceholderRules, Pcre2PlaceholderService, PlaceholderRuleCompilationError,
    PlaceholderRuleDefinition,
};
use crate::rpg_maker::translate::semantics::{
    GroupContextFingerprintError, ManualTranslationStateError, ResolvedTranslationSemanticError,
    TranslationResourceFacts, group_context_fingerprint_with_cancellation,
    manual_translation_state_fingerprint_with_cancellation,
    prepare_translation_resource_facts_with_cancellation,
};
use crate::rpg_maker::write_back::planner::{RpgMakerWriteBackGroup, RpgMakerWriteBackUnit};
use crate::storage::sqlite::{SqliteRow, SqliteValue};
use crate::translation::placeholder::PlaceholderProtectionError;
#[cfg(test)]
use crate::translation::placeholder::PlaceholderWorkerOperation;
use crate::translation::planning_resource::{
    CompiledTerminology, TerminologyDefinitionError, TerminologyEntry,
    compile_terminology_with_cancellation,
};

use super::{
    ProjectLuaCallError, ProjectLuaCancellation, ProjectLuaDatabasePrerequisiteError,
    ProjectLuaEngineAdapter, ProjectLuaProject, ProjectLuaSchemaObjectKind, ProjectLuaValue,
    project_lua_object_contains_field, take_project_lua_object_field,
};

fn rpg_maker_call_violation(
    violation: crate::diagnostic::LuaValueViolation,
) -> ProjectLuaCallError {
    ProjectLuaCallError::violation(violation).with_engine(crate::diagnostic::LuaEngine::RpgMaker)
}

fn rpg_maker_state_error() -> ProjectLuaCallError {
    rpg_maker_call_violation(crate::diagnostic::LuaValueViolation::StateMismatch)
}

fn rpg_maker_sqlite_error(source: rusqlite::Error) -> ProjectLuaCallError {
    ProjectLuaCallError::sqlite(source).with_engine(crate::diagnostic::LuaEngine::RpgMaker)
}

const RPG_MAKER_ATT_TABLES: &[&str] = &[
    "metadata",
    "init_run_plan",
    "extract_run_plan",
    "extract_rules_definition",
    "translate_run_plan",
    "rpg_maker_asset_owner_state",
    "rpg_maker_text_group",
    "rpg_maker_text_unit",
    "rpg_maker_mutation_claim",
    "rpg_maker_translation_resource",
    "rpg_maker_project_definition",
];

/// RPG Maker 项目数据库的 typed translation 与最终校验。
#[derive(Debug)]
pub(crate) struct RpgMakerProjectLuaAdapter {
    engine: RpgMakerEngine,
    cancellation: Arc<dyn RpgMakerLuaCancellationProbe>,
    translation_baseline: Mutex<Option<RpgMakerTranslationBaseline>>,
}

impl RpgMakerProjectLuaAdapter {
    pub(crate) fn new(engine: RpgMakerEngine, cancellation: ProjectLuaCancellation) -> Self {
        Self::with_cancellation_probe(engine, Arc::new(cancellation))
    }

    fn with_cancellation_probe(
        engine: RpgMakerEngine,
        cancellation: Arc<dyn RpgMakerLuaCancellationProbe>,
    ) -> Self {
        Self {
            engine,
            cancellation,
            translation_baseline: Mutex::new(None),
        }
    }

    fn cancellation(&self, phase: RpgMakerLuaCancellationPhase) -> RpgMakerLuaCancellation<'_> {
        RpgMakerLuaCancellation {
            probe: self.cancellation.as_ref(),
            phase,
        }
    }

    fn placeholder_rules_for_resource(
        &self,
        canonical_json: String,
        cancellation: RpgMakerLuaCancellation<'_>,
    ) -> Result<(Pcre2PlaceholderService, CompiledPlaceholderRules), ProjectLuaCallError> {
        cancellation.ensure_running()?;
        let service = {
            let baseline = self
                .translation_baseline
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let cache = &baseline
                .as_ref()
                .ok_or_else(rpg_maker_state_error)?
                .placeholder_cache;
            if text_eq_with_cancellation(&cache.canonical_json, &canonical_json, cancellation)? {
                return Ok((cache.service.clone(), cache.rules.clone()));
            }
            cache.service.clone()
        };

        let rules = compile_placeholder_rules(&service, &canonical_json, cancellation)?;
        cancellation.ensure_running()?;
        let mut baseline = self
            .translation_baseline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cache = &mut baseline
            .as_mut()
            .ok_or_else(rpg_maker_state_error)?
            .placeholder_cache;
        if text_eq_with_cancellation(&cache.canonical_json, &canonical_json, cancellation)? {
            return Ok((cache.service.clone(), cache.rules.clone()));
        }
        cache.canonical_json = canonical_json;
        cache.service = service.clone();
        cache.rules = rules.clone();
        cancellation.ensure_running()?;
        Ok((service, rules))
    }
}

/// 为命令接线建立 RPG Maker Lua 引擎适配器。
pub(crate) fn rpg_maker_project_lua_adapter(
    engine: RpgMakerEngine,
    cancellation: ProjectLuaCancellation,
) -> Arc<dyn ProjectLuaEngineAdapter> {
    Arc::new(RpgMakerProjectLuaAdapter::new(engine, cancellation))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RpgMakerLuaCancellationPhase {
    Capture,
    ScriptCall,
    Validation,
}

trait RpgMakerLuaCancellationProbe: fmt::Debug + Send + Sync {
    fn ensure_running(
        &self,
        phase: RpgMakerLuaCancellationPhase,
    ) -> Result<(), ProjectLuaCallError>;
}

impl RpgMakerLuaCancellationProbe for ProjectLuaCancellation {
    fn ensure_running(
        &self,
        _phase: RpgMakerLuaCancellationPhase,
    ) -> Result<(), ProjectLuaCallError> {
        if self.is_cancelled() {
            Err(rpg_maker_lua_cancelled())
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy)]
struct RpgMakerLuaCancellation<'a> {
    probe: &'a dyn RpgMakerLuaCancellationProbe,
    phase: RpgMakerLuaCancellationPhase,
}

impl RpgMakerLuaCancellation<'_> {
    fn ensure_running(self) -> Result<(), ProjectLuaCallError> {
        self.probe.ensure_running(self.phase)
    }
}

impl ProjectLuaEngineAdapter for RpgMakerProjectLuaAdapter {
    fn protects_schema_object(
        &self,
        kind: ProjectLuaSchemaObjectKind,
        name: &str,
        table_name: &str,
    ) -> bool {
        let protected_table = |candidate: &str| {
            RPG_MAKER_ATT_TABLES
                .iter()
                .any(|table| table.eq_ignore_ascii_case(candidate))
        };
        match kind {
            ProjectLuaSchemaObjectKind::Table => protected_table(name),
            ProjectLuaSchemaObjectKind::Index
            | ProjectLuaSchemaObjectKind::View
            | ProjectLuaSchemaObjectKind::Trigger => protected_table(table_name),
        }
    }

    fn set_translation(
        &self,
        connection: &Connection,
        locator: ProjectLuaValue,
        translation: ProjectLuaValue,
    ) -> Result<u64, ProjectLuaCallError> {
        let cancellation = self.cancellation(RpgMakerLuaCancellationPhase::ScriptCall);
        cancellation.ensure_running()?;
        let locator = parse_locator(locator)?;
        let unit = load_unit(connection, &locator, cancellation)?;
        let group_contexts = load_group_context_fingerprints(
            connection,
            Some(&locator.group_location),
            cancellation,
        )?;
        let group_context = group_contexts
            .get(&locator.group_location)
            .copied()
            .ok_or_else(rpg_maker_state_error)?;
        let LoadedUnit {
            owner,
            group_location,
            role,
            kind,
            source_content,
            source_context_json,
            language_pair,
            placeholder_rules_json,
        } = unit;
        cancellation.ensure_running()?;
        let translation = parse_translation(translation, &source_content, cancellation)?;
        validate_translation_structure(kind, &role, &source_content, &translation, cancellation)?;
        let (placeholder_service, placeholder_rules) =
            self.placeholder_rules_for_resource(placeholder_rules_json, cancellation)?;
        let placeholders = validate_manual_placeholders_with_rules(
            &placeholder_service,
            &placeholder_rules,
            self.engine,
            kind,
            &source_content,
            &translation,
            cancellation,
        )?;
        cancellation.ensure_running()?;
        let identity = TranslationUnitIdentity::new(
            owner,
            kind,
            group_location,
            role,
            source_content,
            source_context_json,
        );
        let state = manual_translation_state_fingerprint_with_cancellation(
            self.engine,
            &language_pair,
            group_context,
            &identity,
            &placeholders,
            || cancellation.ensure_running(),
        )?
        .map_err(manual_state_error)?;
        cancellation.ensure_running()?;
        let translation_json =
            encode_json_with_cancellation(&translation, "RPG Maker 人工译文", cancellation)
                .map_err(|source| {
                    if source.kind() == "cancelled" {
                        source
                    } else {
                        rpg_maker_call_violation(
                            crate::diagnostic::LuaValueViolation::InvalidTranslation,
                        )
                    }
                })?;
        cancellation.ensure_running()?;
        let changed = connection
            .execute(
                "UPDATE main.rpg_maker_text_unit
                 SET translation_content_json = ?1,
                     translation_state = ?2
                 WHERE owner = ?3
                   AND group_id = (
                       SELECT group_id
                       FROM main.rpg_maker_text_group
                       WHERE owner = ?3 AND group_location = ?4
                   )
                   AND unit_role = ?5",
                params![
                    translation_json,
                    state.as_bytes().as_slice(),
                    locator.owner.storage_name(),
                    locator.group_location,
                    locator.unit_role
                ],
            )
            .map_err(rpg_maker_sqlite_error)?;
        if changed != 1 {
            return Err(rpg_maker_call_violation(
                crate::diagnostic::LuaValueViolation::UnknownUnit,
            )
            .with_rpg_maker_locator(
                locator.owner.storage_name(),
                &locator.group_location,
                &locator.unit_role,
            ));
        }
        Ok(u64::try_from(changed).expect("受支持平台的 usize 必须能表示为 u64"))
    }

    fn clear_translation(
        &self,
        connection: &Connection,
        locator: ProjectLuaValue,
    ) -> Result<u64, ProjectLuaCallError> {
        let cancellation = self.cancellation(RpgMakerLuaCancellationPhase::ScriptCall);
        cancellation.ensure_running()?;
        let locator = parse_locator(locator)?;
        let changed = connection
            .execute(
                "UPDATE main.rpg_maker_text_unit
                 SET translation_content_json = NULL,
                     translation_state = NULL
                 WHERE owner = ?1
                   AND group_id = (
                       SELECT group_id
                       FROM main.rpg_maker_text_group
                       WHERE owner = ?1 AND group_location = ?2
                   )
                   AND unit_role = ?3",
                params![
                    locator.owner.storage_name(),
                    locator.group_location,
                    locator.unit_role
                ],
            )
            .map_err(rpg_maker_sqlite_error)?;
        if changed != 1 {
            return Err(rpg_maker_call_violation(
                crate::diagnostic::LuaValueViolation::UnknownUnit,
            )
            .with_rpg_maker_locator(
                locator.owner.storage_name(),
                &locator.group_location,
                &locator.unit_role,
            ));
        }
        Ok(u64::try_from(changed).expect("受支持平台的 usize 必须能表示为 u64"))
    }

    fn validate_database_before_script(
        &self,
        connection: &Connection,
        _project: &ProjectLuaProject,
    ) -> Result<(), ProjectLuaDatabasePrerequisiteError> {
        let cancellation = self.cancellation(RpgMakerLuaCancellationPhase::Capture);
        match validate_current_att_schema_with_cancellation(connection, || {
            cancellation.ensure_running().is_err()
        }) {
            Ok(()) => Ok(()),
            Err(CurrentAttSchemaValidationError::Cancelled) => {
                Err(ProjectLuaDatabasePrerequisiteError::Cancelled)
            }
            Err(CurrentAttSchemaValidationError::Read(source)) => {
                Err(ProjectLuaDatabasePrerequisiteError::sqlite(
                    super::ProjectLuaSqliteOperation::ReadCurrentAttSchema,
                    source,
                ))
            }
            Err(CurrentAttSchemaValidationError::Invalid(_reason)) => {
                Err(ProjectLuaDatabasePrerequisiteError::invalid_project_state(
                    crate::diagnostic::LuaEngine::RpgMaker,
                    crate::diagnostic::LuaValueViolation::StateMismatch,
                ))
            }
        }
    }

    fn capture_database_state(
        &self,
        connection: &Connection,
        _project: &ProjectLuaProject,
    ) -> Result<(), ProjectLuaCallError> {
        let cancellation = self.cancellation(RpgMakerLuaCancellationPhase::Capture);
        cancellation.ensure_running()?;
        let baseline =
            capture_rpg_maker_translation_baseline(connection, self.engine, cancellation)?;
        cancellation.ensure_running()?;
        let mut slot = self
            .translation_baseline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_some() {
            return Err(rpg_maker_state_error());
        }
        *slot = Some(baseline);
        cancellation.ensure_running()?;
        Ok(())
    }

    fn validate_database(
        &self,
        connection: &Connection,
        project: &ProjectLuaProject,
    ) -> Result<(), ProjectLuaCallError> {
        let cancellation = self.cancellation(RpgMakerLuaCancellationPhase::Validation);
        cancellation.ensure_running()?;
        let baseline = self
            .translation_baseline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let baseline = baseline.as_ref().ok_or_else(rpg_maker_state_error)?;
        validate_rpg_maker_project(
            connection,
            project.name(),
            self.engine,
            baseline,
            cancellation,
        )
    }
}

#[derive(Debug)]
struct RpgMakerCurrentBaseline {
    group_kind: String,
    group_context: GroupContextFingerprint,
    source_content_json: String,
    source_context_json: String,
    translation_content_json: String,
    translation_state: Sha256Fingerprint,
    placeholders: PlaceholderMultiset,
    origin: RpgMakerTranslationOrigin,
}

#[derive(Debug)]
enum RpgMakerTranslationOrigin {
    Manual,
    Automatic(TerminologyDependencyProof),
}

#[derive(Debug)]
struct RpgMakerTranslationBaseline {
    source_language: String,
    target_language: String,
    currents: RpgMakerCurrentBaselines,
    placeholder_cache: RpgMakerPlaceholderCache,
    terminology_cache: RpgMakerTerminologyCache,
}

#[derive(Debug)]
struct RpgMakerUnitKey {
    fingerprint: Sha256Fingerprint,
    owner: String,
    group_location: String,
    unit_role: String,
}

#[derive(Debug)]
struct StoredRpgMakerCurrent {
    key: RpgMakerUnitKey,
    baseline: RpgMakerCurrentBaseline,
}

#[derive(Debug, Default)]
struct RpgMakerCurrentBaselines {
    buckets: HashMap<Sha256Fingerprint, Vec<StoredRpgMakerCurrent>>,
}

impl RpgMakerCurrentBaselines {
    fn insert(
        &mut self,
        owner: String,
        group_location: String,
        unit_role: String,
        baseline: RpgMakerCurrentBaseline,
        cancellation: RpgMakerLuaCancellation<'_>,
    ) -> Result<bool, ProjectLuaCallError> {
        let fingerprint = unit_key_fingerprint(&owner, &group_location, &unit_role, cancellation)?;
        let key = RpgMakerUnitKey {
            fingerprint,
            owner,
            group_location,
            unit_role,
        };
        let bucket = self.buckets.entry(fingerprint).or_default();
        for existing in bucket.iter() {
            cancellation.ensure_running()?;
            if unit_key_eq_with_cancellation(&existing.key, &key, cancellation)? {
                return Ok(false);
            }
        }
        bucket.push(StoredRpgMakerCurrent { key, baseline });
        cancellation.ensure_running()?;
        Ok(true)
    }

    fn get(
        &self,
        owner: &str,
        group_location: &str,
        unit_role: &str,
        cancellation: RpgMakerLuaCancellation<'_>,
    ) -> Result<Option<&RpgMakerCurrentBaseline>, ProjectLuaCallError> {
        let fingerprint = unit_key_fingerprint(owner, group_location, unit_role, cancellation)?;
        let Some(bucket) = self.buckets.get(&fingerprint) else {
            return Ok(None);
        };
        for existing in bucket {
            cancellation.ensure_running()?;
            if unit_key_parts_eq_with_cancellation(
                &existing.key,
                owner,
                group_location,
                unit_role,
                cancellation,
            )? {
                return Ok(Some(&existing.baseline));
            }
        }
        cancellation.ensure_running()?;
        Ok(None)
    }
}

struct StoredRpgMakerGroupIndex {
    owner: RpgMakerAssetOwner,
    location: String,
    index: usize,
}

#[derive(Default)]
struct RpgMakerGroupIndexes {
    buckets: HashMap<Sha256Fingerprint, Vec<StoredRpgMakerGroupIndex>>,
}

impl RpgMakerGroupIndexes {
    fn insert(
        &mut self,
        owner: RpgMakerAssetOwner,
        location: String,
        index: usize,
        cancellation: RpgMakerLuaCancellation<'_>,
    ) -> Result<bool, ProjectLuaCallError> {
        let fingerprint = group_key_fingerprint(owner, &location, cancellation)?;
        let bucket = self.buckets.entry(fingerprint).or_default();
        for existing in bucket.iter() {
            cancellation.ensure_running()?;
            if existing.owner == owner
                && text_eq_with_cancellation(&existing.location, &location, cancellation)?
            {
                return Ok(false);
            }
        }
        bucket.push(StoredRpgMakerGroupIndex {
            owner,
            location,
            index,
        });
        cancellation.ensure_running()?;
        Ok(true)
    }

    fn get(
        &self,
        owner: RpgMakerAssetOwner,
        location: &str,
        cancellation: RpgMakerLuaCancellation<'_>,
    ) -> Result<Option<usize>, ProjectLuaCallError> {
        let fingerprint = group_key_fingerprint(owner, location, cancellation)?;
        let Some(bucket) = self.buckets.get(&fingerprint) else {
            return Ok(None);
        };
        for existing in bucket {
            cancellation.ensure_running()?;
            if existing.owner == owner
                && text_eq_with_cancellation(&existing.location, location, cancellation)?
            {
                return Ok(Some(existing.index));
            }
        }
        cancellation.ensure_running()?;
        Ok(None)
    }
}

struct RpgMakerPlaceholderCache {
    canonical_json: String,
    service: Pcre2PlaceholderService,
    rules: CompiledPlaceholderRules,
}

struct RpgMakerTerminologyCache {
    canonical_json: String,
    terminology: Arc<CompiledTerminology>,
}

impl fmt::Debug for RpgMakerTerminologyCache {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RpgMakerTerminologyCache")
            .field("canonical_json_bytes", &self.canonical_json.len())
            .field("entries", &self.terminology.entries().len())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for RpgMakerPlaceholderCache {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RpgMakerPlaceholderCache")
            .field("canonical_json_bytes", &self.canonical_json.len())
            .field("rules", &self.rules)
            .finish_non_exhaustive()
    }
}

struct ParsedLocator {
    owner: RpgMakerAssetOwner,
    group_location: String,
    unit_role: String,
}

fn parse_locator(locator: ProjectLuaValue) -> Result<ParsedLocator, ProjectLuaCallError> {
    let Some(mut fields) = locator.into_object() else {
        return Err(rpg_maker_call_violation(
            crate::diagnostic::LuaValueViolation::InvalidLocator,
        ));
    };
    if fields.len() != 3
        || !project_lua_object_contains_field(&fields, "owner")
        || !project_lua_object_contains_field(&fields, "group_location")
        || !project_lua_object_contains_field(&fields, "unit_role")
    {
        return Err(rpg_maker_call_violation(
            crate::diagnostic::LuaValueViolation::InvalidLocator,
        ));
    }
    let owner_raw = take_locator_text(&mut fields, "owner")?;
    let owner = RpgMakerAssetOwner::from_storage_name(&owner_raw).ok_or_else(|| {
        rpg_maker_call_violation(crate::diagnostic::LuaValueViolation::InvalidLocator)
            .with_field("owner")
    })?;
    let group_location = take_locator_text(&mut fields, "group_location")?;
    RpgMakerLocationCodec::decode(&group_location).map_err(|_| {
        rpg_maker_call_violation(crate::diagnostic::LuaValueViolation::InvalidLocator)
            .with_field("group_location")
    })?;
    let unit_role = take_locator_text(&mut fields, "unit_role")?;
    RpgMakerProjectionCodec::decode_role(&unit_role).map_err(|_| {
        rpg_maker_call_violation(crate::diagnostic::LuaValueViolation::InvalidLocator)
            .with_field("unit_role")
    })?;
    Ok(ParsedLocator {
        owner,
        group_location,
        unit_role,
    })
}

fn take_locator_text(
    fields: &mut Vec<(String, ProjectLuaValue)>,
    field: &'static str,
) -> Result<String, ProjectLuaCallError> {
    let Some(value) =
        take_project_lua_object_field(fields, field).and_then(ProjectLuaValue::into_text)
    else {
        return Err(
            rpg_maker_call_violation(crate::diagnostic::LuaValueViolation::InvalidLocator)
                .with_field(field),
        );
    };
    if value.is_empty() || value.chars().all(char::is_whitespace) {
        return Err(
            rpg_maker_call_violation(crate::diagnostic::LuaValueViolation::InvalidLocator)
                .with_field(field),
        );
    }
    Ok(value)
}

struct LoadedUnit {
    owner: RpgMakerAssetOwner,
    group_location: crate::rpg_maker::text::RpgMakerLocation,
    role: TextUnitRole,
    kind: TextGroupKind,
    source_content: TextUnitContent,
    source_context_json: String,
    language_pair: LanguagePair,
    placeholder_rules_json: String,
}

struct LoadedGroupContextUnit {
    semantic_order_key: RpgMakerSemanticOrderKey,
    identity: TranslationUnitIdentity,
}

struct LoadedGroupContext {
    location_raw: String,
    location: crate::rpg_maker::text::RpgMakerLocation,
    kind: TextGroupKind,
    semantic_order_key: RpgMakerSemanticOrderKey,
    units: Vec<LoadedGroupContextUnit>,
}

fn load_group_context_fingerprints(
    connection: &Connection,
    only_location: Option<&str>,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<HashMap<String, GroupContextFingerprint>, ProjectLuaCallError> {
    cancellation.ensure_running()?;
    let group_sql = if only_location.is_some() {
        "SELECT owner, group_location, semantic_order_key, group_kind
         FROM main.rpg_maker_text_group
         WHERE group_location = ?1
         ORDER BY semantic_order_key,
                  CASE owner WHEN 'builtin' THEN 0 WHEN 'rules' THEN 1 END"
    } else {
        "SELECT owner, group_location, semantic_order_key, group_kind
         FROM main.rpg_maker_text_group
         ORDER BY semantic_order_key,
                  CASE owner WHEN 'builtin' THEN 0 WHEN 'rules' THEN 1 END"
    };
    let mut group_statement = connection
        .prepare(group_sql)
        .map_err(|_| rpg_maker_state_error())?;
    let mut group_rows = match only_location {
        Some(location) => group_statement.query(params![location]),
        None => group_statement.query([]),
    }
    .map_err(|_| rpg_maker_state_error())?;
    let mut group_indexes = RpgMakerGroupIndexes::default();
    let mut logical_locations = HashMap::<String, usize>::new();
    let mut semantic_orders = HashMap::<RpgMakerSemanticOrderKey, String>::new();
    let mut groups = Vec::<LoadedGroupContext>::new();
    while let Some(row) = group_rows.next().map_err(|_| rpg_maker_state_error())? {
        cancellation.ensure_running()?;
        let owner_raw =
            sqlite_text_with_cancellation(row, 0, "owner", cancellation).map_err(|source| {
                invalid_database_sqlite_read_error(source, "读取完整 Group 语境失败")
            })?;
        let location_raw = sqlite_text_with_cancellation(row, 1, "group_location", cancellation)
            .map_err(|source| {
                invalid_database_sqlite_read_error(source, "读取完整 Group 语境失败")
            })?;
        let semantic_order_key =
            sqlite_blob_with_cancellation(row, 2, "semantic_order_key", cancellation).map_err(
                |source| invalid_database_sqlite_read_error(source, "读取完整 Group 语境失败"),
            )?;
        let kind_raw = sqlite_text_with_cancellation(row, 3, "group_kind", cancellation).map_err(
            |source| invalid_database_sqlite_read_error(source, "读取完整 Group 语境失败"),
        )?;
        let owner =
            RpgMakerAssetOwner::from_storage_name(&owner_raw).ok_or_else(rpg_maker_state_error)?;
        let semantic_order_key = RpgMakerSemanticOrderKey::decode(&semantic_order_key)
            .map_err(|_| rpg_maker_state_error())?;
        let kind = TextGroupKind::from_storage_name(&kind_raw).ok_or_else(rpg_maker_state_error)?;
        let location =
            RpgMakerLocationCodec::decode(&location_raw).map_err(|_| rpg_maker_state_error())?;
        let index = if let Some(index) = logical_locations.get(&location_raw).copied() {
            let existing = &groups[index];
            if existing.kind != kind
                || existing.semantic_order_key != semantic_order_key
                || existing.location != location
            {
                return Err(rpg_maker_state_error());
            }
            index
        } else {
            if semantic_orders
                .insert(semantic_order_key.clone(), location_raw.clone())
                .is_some()
            {
                return Err(rpg_maker_state_error());
            }
            let index = groups.len();
            logical_locations.insert(location_raw.clone(), index);
            groups.push(LoadedGroupContext {
                location_raw: location_raw.clone(),
                location,
                kind,
                semantic_order_key,
                units: Vec::new(),
            });
            index
        };
        if !group_indexes.insert(owner, location_raw, index, cancellation)? {
            return Err(rpg_maker_state_error());
        }
    }
    drop(group_rows);
    drop(group_statement);

    cancellation.ensure_running()?;
    let unit_sql = if only_location.is_some() {
        "SELECT unit.owner, text_group.group_location, unit.unit_role,
                unit.semantic_order_key, unit.source_content_json,
                unit.source_context_json
         FROM main.rpg_maker_text_unit AS unit
         JOIN main.rpg_maker_text_group AS text_group
           ON text_group.owner = unit.owner
          AND text_group.group_id = unit.group_id
         WHERE text_group.group_location = ?1
         ORDER BY unit.semantic_order_key,
                  CASE unit.owner WHEN 'builtin' THEN 0 WHEN 'rules' THEN 1 END"
    } else {
        "SELECT unit.owner, text_group.group_location, unit.unit_role,
                unit.semantic_order_key, unit.source_content_json,
                unit.source_context_json
         FROM main.rpg_maker_text_unit AS unit
         JOIN main.rpg_maker_text_group AS text_group
           ON text_group.owner = unit.owner
          AND text_group.group_id = unit.group_id
         ORDER BY unit.semantic_order_key,
                  CASE unit.owner WHEN 'builtin' THEN 0 WHEN 'rules' THEN 1 END"
    };
    let mut unit_statement = connection
        .prepare(unit_sql)
        .map_err(|_| rpg_maker_state_error())?;
    let mut unit_rows = match only_location {
        Some(location) => unit_statement.query(params![location]),
        None => unit_statement.query([]),
    }
    .map_err(|_| rpg_maker_state_error())?;
    while let Some(row) = unit_rows.next().map_err(|_| rpg_maker_state_error())? {
        cancellation.ensure_running()?;
        let owner_raw =
            sqlite_text_with_cancellation(row, 0, "owner", cancellation).map_err(|source| {
                invalid_database_sqlite_read_error(source, "读取完整 Group Unit 失败")
            })?;
        let location_raw = sqlite_text_with_cancellation(row, 1, "group_location", cancellation)
            .map_err(|source| {
                invalid_database_sqlite_read_error(source, "读取完整 Group Unit 失败")
            })?;
        let role_raw =
            sqlite_text_with_cancellation(row, 2, "unit_role", cancellation).map_err(|source| {
                invalid_database_sqlite_read_error(source, "读取完整 Group Unit 失败")
            })?;
        let semantic_order_key =
            sqlite_blob_with_cancellation(row, 3, "semantic_order_key", cancellation).map_err(
                |source| invalid_database_sqlite_read_error(source, "读取完整 Group Unit 失败"),
            )?;
        let source_json =
            sqlite_text_with_cancellation(row, 4, "source_content_json", cancellation).map_err(
                |source| invalid_database_sqlite_read_error(source, "读取完整 Group Unit 失败"),
            )?;
        let context_json =
            sqlite_text_with_cancellation(row, 5, "source_context_json", cancellation).map_err(
                |source| invalid_database_sqlite_read_error(source, "读取完整 Group Unit 失败"),
            )?;
        let owner =
            RpgMakerAssetOwner::from_storage_name(&owner_raw).ok_or_else(rpg_maker_state_error)?;
        let index = group_indexes
            .get(owner, &location_raw, cancellation)?
            .ok_or_else(rpg_maker_state_error)?;
        let semantic_order_key = RpgMakerSemanticOrderKey::decode(&semantic_order_key)
            .map_err(|_| rpg_maker_state_error())?;
        let role =
            RpgMakerProjectionCodec::decode_role(&role_raw).map_err(|_| rpg_maker_state_error())?;
        let source_content: TextUnitContent =
            parse_json_with_cancellation(&source_json, "完整 Group Unit 原文", cancellation)?;
        let context: serde_json::Value =
            parse_json_with_cancellation(&context_json, "完整 Group Unit 上下文", cancellation)?;
        if !context.is_object() {
            return Err(rpg_maker_state_error());
        }
        let group = &mut groups[index];
        if group
            .units
            .iter()
            .any(|unit| unit.semantic_order_key == semantic_order_key)
        {
            return Err(rpg_maker_state_error());
        }
        if group.units.iter().any(|unit| unit.identity.role() == &role) {
            return Err(rpg_maker_state_error());
        }
        group.units.push(LoadedGroupContextUnit {
            semantic_order_key,
            identity: TranslationUnitIdentity::new(
                owner,
                group.kind,
                group.location.clone(),
                role,
                source_content,
                context_json,
            ),
        });
    }
    drop(unit_rows);
    drop(unit_statement);

    let mut fingerprints = HashMap::with_capacity(groups.len());
    for mut group in groups {
        cancellation.ensure_running()?;
        if group.units.is_empty() {
            return Err(rpg_maker_state_error());
        }
        group
            .units
            .sort_by(|left, right| left.semantic_order_key.cmp(&right.semantic_order_key));
        let fingerprint = group_context_fingerprint_with_cancellation(
            group.kind,
            &group.semantic_order_key,
            group
                .units
                .iter()
                .map(|unit| (&unit.semantic_order_key, &unit.identity)),
            || cancellation.ensure_running(),
        )?
        .map_err(group_context_fingerprint_error)?;
        if fingerprints
            .insert(group.location_raw, fingerprint)
            .is_some()
        {
            return Err(rpg_maker_state_error());
        }
    }
    cancellation.ensure_running()?;
    Ok(fingerprints)
}

fn group_context_fingerprint_error(_source: GroupContextFingerprintError) -> ProjectLuaCallError {
    rpg_maker_state_error()
}

fn load_unit(
    connection: &Connection,
    locator: &ParsedLocator,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<LoadedUnit, ProjectLuaCallError> {
    cancellation.ensure_running()?;
    type UnitRow = (String, String, String, String, String, String, String);
    let mut statement = connection
        .prepare(
            "SELECT text_group.group_kind,
                    text_unit.source_content_json,
                    text_unit.source_context_json,
                    metadata.source_language,
                    metadata.target_language,
                    resource.canonical_json,
                    text_unit.owner
             FROM main.rpg_maker_text_unit AS text_unit
              JOIN main.rpg_maker_text_group AS text_group
                ON text_group.owner = text_unit.owner
               AND text_group.group_id = text_unit.group_id
             CROSS JOIN main.metadata
             JOIN main.rpg_maker_translation_resource AS resource
               ON resource.resource_kind = 'placeholder_rules'
              WHERE text_unit.owner = ?1
                AND text_group.group_location = ?2
                AND text_unit.unit_role = ?3",
        )
        .map_err(rpg_maker_sqlite_error)?;
    let mut rows = statement
        .query(params![
            locator.owner.storage_name(),
            locator.group_location,
            locator.unit_role
        ])
        .map_err(rpg_maker_sqlite_error)?;
    let row: Option<UnitRow> = match rows.next().map_err(rpg_maker_sqlite_error)? {
        Some(row) => Some((
            sqlite_text_with_cancellation(row, 0, "group_kind", cancellation)
                .map_err(|source| lua_sqlite_read_error(source, "读取 RPG Maker Lua Unit 失败"))?,
            sqlite_text_with_cancellation(row, 1, "source_content_json", cancellation)
                .map_err(|source| lua_sqlite_read_error(source, "读取 RPG Maker Lua Unit 失败"))?,
            sqlite_text_with_cancellation(row, 2, "source_context_json", cancellation)
                .map_err(|source| lua_sqlite_read_error(source, "读取 RPG Maker Lua Unit 失败"))?,
            sqlite_text_with_cancellation(row, 3, "source_language", cancellation)
                .map_err(|source| lua_sqlite_read_error(source, "读取 RPG Maker Lua Unit 失败"))?,
            sqlite_text_with_cancellation(row, 4, "target_language", cancellation)
                .map_err(|source| lua_sqlite_read_error(source, "读取 RPG Maker Lua Unit 失败"))?,
            sqlite_text_with_cancellation(row, 5, "canonical_json", cancellation)
                .map_err(|source| lua_sqlite_read_error(source, "读取 RPG Maker Lua Unit 失败"))?,
            sqlite_text_with_cancellation(row, 6, "owner", cancellation)
                .map_err(|source| lua_sqlite_read_error(source, "读取 RPG Maker Lua Unit 失败"))?,
        )),
        None => None,
    };
    drop(rows);
    drop(statement);
    cancellation.ensure_running()?;
    let Some((
        kind_raw,
        source_content_json,
        source_context_json,
        source_language,
        target_language,
        placeholder_rules_json,
        owner_raw,
    )) = row
    else {
        return Err(
            rpg_maker_call_violation(crate::diagnostic::LuaValueViolation::UnknownUnit)
                .with_rpg_maker_locator(
                    locator.owner.storage_name(),
                    &locator.group_location,
                    &locator.unit_role,
                ),
        );
    };
    let owner =
        RpgMakerAssetOwner::from_storage_name(&owner_raw).ok_or_else(rpg_maker_state_error)?;
    let kind = TextGroupKind::from_storage_name(&kind_raw).ok_or_else(rpg_maker_state_error)?;
    let group_location = RpgMakerLocationCodec::decode(&locator.group_location)
        .map_err(|_| rpg_maker_state_error())?;
    let role = RpgMakerProjectionCodec::decode_role(&locator.unit_role)
        .map_err(|_| rpg_maker_state_error())?;
    let source_content: TextUnitContent = parse_json_with_cancellation(
        &source_content_json,
        "Unit source_content_json",
        cancellation,
    )?;
    cancellation.ensure_running()?;
    validate_text_unit_content_structure(kind, &role, TextUnitContentView::from(&source_content))
        .map_err(|_| rpg_maker_state_error())?;
    if content_is_blank(&source_content, cancellation)? {
        return Err(rpg_maker_state_error());
    }
    let context: serde_json::Value = parse_json_with_cancellation(
        &source_context_json,
        "Unit source_context_json",
        cancellation,
    )?;
    if !context.is_object() {
        return Err(rpg_maker_state_error());
    }
    let source = parse_canonical_language(&source_language, "source_language")?;
    let target = parse_canonical_language(&target_language, "target_language")?;
    cancellation.ensure_running()?;
    Ok(LoadedUnit {
        owner,
        group_location,
        role,
        kind,
        source_content,
        source_context_json,
        language_pair: LanguagePair::new(source, target),
        placeholder_rules_json,
    })
}

fn parse_translation(
    value: ProjectLuaValue,
    source: &TextUnitContent,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<TextUnitContent, ProjectLuaCallError> {
    cancellation.ensure_running()?;
    match source {
        TextUnitContent::Value(_) => {
            let Some(value) = value.into_text() else {
                return Err(rpg_maker_call_violation(
                    crate::diagnostic::LuaValueViolation::InvalidTranslation,
                ));
            };
            cancellation.ensure_running()?;
            Ok(TextUnitContent::Value(value))
        }
        TextUnitContent::Lines(_) => {
            let Some(values) = value.into_array() else {
                return Err(rpg_maker_call_violation(
                    crate::diagnostic::LuaValueViolation::InvalidTranslation,
                ));
            };
            let mut lines = Vec::with_capacity(values.len());
            for value in values {
                cancellation.ensure_running()?;
                let Some(value) = value.into_text() else {
                    return Err(rpg_maker_call_violation(
                        crate::diagnostic::LuaValueViolation::InvalidTranslation,
                    ));
                };
                lines.push(value);
            }
            cancellation.ensure_running()?;
            Ok(TextUnitContent::Lines(lines))
        }
    }
}

fn validate_translation_structure(
    kind: TextGroupKind,
    role: &TextUnitRole,
    source: &TextUnitContent,
    translation: &TextUnitContent,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<(), ProjectLuaCallError> {
    cancellation.ensure_running()?;
    if content_is_blank(translation, cancellation)? {
        return Err(rpg_maker_call_violation(
            crate::diagnostic::LuaValueViolation::InvalidTranslation,
        ));
    }
    let contains_forbidden = content_contains_forbidden(translation, cancellation)?;
    if contains_forbidden {
        return Err(rpg_maker_call_violation(
            crate::diagnostic::LuaValueViolation::InvalidTranslation,
        ));
    }
    validate_text_unit_content_structure(kind, role, TextUnitContentView::from(translation))
        .map_err(|_| {
            rpg_maker_call_violation(crate::diagnostic::LuaValueViolation::InvalidTranslation)
        })?;
    cancellation.ensure_running()?;
    if matches!(role, TextUnitRole::Choices | TextUnitRole::ScrollingText) {
        let source_lines = source.as_lines().expect("严格对齐角色的原文必须是行数组");
        let translation_lines = translation
            .as_lines()
            .expect("严格对齐角色的译文必须是行数组");
        if source_lines.len() != translation_lines.len() {
            return Err(rpg_maker_call_violation(
                crate::diagnostic::LuaValueViolation::InvalidTranslation,
            ));
        }
        for (source, translation) in source_lines.iter().zip(translation_lines) {
            cancellation.ensure_running()?;
            if text_is_blank(source, cancellation)? != text_is_blank(translation, cancellation)? {
                return Err(rpg_maker_call_violation(
                    crate::diagnostic::LuaValueViolation::InvalidTranslation,
                ));
            }
        }
    }
    cancellation.ensure_running()
}

fn validate_manual_placeholders_with_rules(
    service: &Pcre2PlaceholderService,
    custom: &CompiledPlaceholderRules,
    engine: RpgMakerEngine,
    kind: TextGroupKind,
    source: &TextUnitContent,
    translation: &TextUnitContent,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<Vec<AppliedPlaceholder>, ProjectLuaCallError> {
    cancellation.ensure_running()?;
    let source_placeholders = protect_content(service, engine, kind, source, custom, cancellation)?;
    validate_translation_placeholders(
        service,
        custom,
        engine,
        kind,
        &source_placeholders,
        translation,
        cancellation,
    )?;
    cancellation.ensure_running()?;
    Ok(source_placeholders)
}

fn validate_translation_placeholders(
    service: &Pcre2PlaceholderService,
    custom: &CompiledPlaceholderRules,
    engine: RpgMakerEngine,
    kind: TextGroupKind,
    source_placeholders: &[AppliedPlaceholder],
    translation: &TextUnitContent,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<(), ProjectLuaCallError> {
    cancellation.ensure_running()?;
    cancellation.ensure_running()?;
    let translation_placeholders =
        protect_content(service, engine, kind, translation, custom, cancellation)?;
    let source_multiset = placeholder_multiset(source_placeholders, cancellation)?;
    let translation_multiset = placeholder_multiset(&translation_placeholders, cancellation)?;
    if !source_multiset.eq_with_cancellation(&translation_multiset, cancellation)? {
        return Err(rpg_maker_call_violation(
            crate::diagnostic::LuaValueViolation::InvalidTranslation,
        ));
    }
    cancellation.ensure_running()?;
    Ok(())
}

fn validate_unchanged_current_placeholders(
    service: &Pcre2PlaceholderService,
    builtin_only_rules: &CompiledPlaceholderRules,
    engine: RpgMakerEngine,
    kind: TextGroupKind,
    source_placeholders: &[AppliedPlaceholder],
    translation: &TextUnitContent,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<(), ProjectLuaCallError> {
    cancellation.ensure_running()?;
    // Translate 恢复 token 后不会重新扫描 Custom 原字节，因为相同字节也可能来自
    // NaturalText。原始 Current 没有保存这项来源信息，因此这里只能重新识别始终具有
    // 控制语义的 Builtin；正文和语义事实均未改变时，Custom 沿用 Translate 已建立的
    // 验收结果。
    let translation_placeholders = protect_content(
        service,
        engine,
        kind,
        translation,
        builtin_only_rules,
        cancellation,
    )?;
    let source_multiset = placeholder_multiset(source_placeholders, cancellation)?;
    let translation_multiset = placeholder_multiset(&translation_placeholders, cancellation)?;
    if !source_multiset.builtin_eq_with_cancellation(&translation_multiset, cancellation)? {
        return Err(rpg_maker_call_violation(
            crate::diagnostic::LuaValueViolation::InvalidTranslation,
        ));
    }
    cancellation.ensure_running()?;
    Ok(())
}

fn compile_placeholder_rules(
    service: &Pcre2PlaceholderService,
    canonical_json: &str,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<CompiledPlaceholderRules, ProjectLuaCallError> {
    let definitions: Vec<PlaceholderRuleDefinition> =
        parse_json_with_cancellation(canonical_json, "Placeholder 资源", cancellation)?;
    let canonical = encode_json_with_cancellation(&definitions, "Placeholder 资源", cancellation)?;
    if !text_eq_with_cancellation(&canonical, canonical_json, cancellation)? {
        return Err(rpg_maker_state_error());
    }
    cancellation.ensure_running()?;
    let compiled = service
        .compile_custom_with_cancellation(definitions, || cancellation.ensure_running())?
        .map_err(placeholder_compilation_error)?;
    cancellation.ensure_running()?;
    Ok(compiled)
}

fn compile_terminology_resource(
    canonical_json: &str,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<Arc<CompiledTerminology>, ProjectLuaCallError> {
    let entries: Vec<TerminologyEntry> =
        parse_json_with_cancellation(canonical_json, "terminology 资源", cancellation)?;
    let canonical = encode_json_with_cancellation(&entries, "terminology", cancellation)?;
    if !text_eq_with_cancellation(&canonical, canonical_json, cancellation)? {
        return Err(rpg_maker_state_error());
    }
    cancellation.ensure_running()?;
    match compile_terminology_with_cancellation(entries, &|| cancellation.ensure_running().is_err())
    {
        Ok(terminology) => {
            cancellation.ensure_running()?;
            Ok(Arc::new(terminology))
        }
        Err(TerminologyDefinitionError::Cancelled) => Err(rpg_maker_lua_cancelled()),
        Err(TerminologyDefinitionError::StartWorker {
            operation: _,
            source,
        }) => Err(ProjectLuaCallError::worker_spawn(source)
            .with_engine(crate::diagnostic::LuaEngine::RpgMaker)),
        Err(_) => Err(rpg_maker_state_error()),
    }
}

fn prepare_current_resource_facts(
    engine: RpgMakerEngine,
    kind: TextGroupKind,
    source: &TextUnitContent,
    terminology: &CompiledTerminology,
    placeholder_service: &Pcre2PlaceholderService,
    placeholder_rules: &CompiledPlaceholderRules,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<TranslationResourceFacts, ProjectLuaCallError> {
    match prepare_translation_resource_facts_with_cancellation(
        engine,
        kind,
        source,
        terminology,
        placeholder_service,
        placeholder_rules,
        || cancellation.ensure_running(),
    )? {
        Ok(facts) => Ok(facts),
        Err(source) => Err(resource_semantic_error(source)),
    }
}

/// 按完整 Group 汇总所有 Unit 实际命中的术语依赖。
///
/// 返回值以 Group 位置为键；依赖按术语文件顺序排列并去重。不同 Group 即使随后
/// 被装进同一个 TaskBlock，也不会在这里互相影响译文状态。
fn load_group_terminology_dependencies(
    connection: &Connection,
    engine: RpgMakerEngine,
    terminology: &CompiledTerminology,
    placeholder_service: &Pcre2PlaceholderService,
    placeholder_rules: &CompiledPlaceholderRules,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<HashMap<String, Vec<TerminologyDependency>>, ProjectLuaCallError> {
    cancellation.ensure_running()?;
    let mut statement = connection
        .prepare(
            "SELECT text_group.group_location, text_group.group_kind,
                    unit.source_content_json
             FROM main.rpg_maker_text_unit AS unit
             JOIN main.rpg_maker_text_group AS text_group
               ON text_group.owner = unit.owner
               AND text_group.group_id = unit.group_id
             ORDER BY text_group.semantic_order_key,
                      unit.semantic_order_key,
                      CASE unit.owner WHEN 'builtin' THEN 0 WHEN 'rules' THEN 1 END",
        )
        .map_err(|_| rpg_maker_state_error())?;
    let mut rows = statement.query([]).map_err(|_| rpg_maker_state_error())?;
    let mut groups =
        HashMap::<String, (TextGroupKind, BTreeMap<usize, TerminologyDependency>)>::new();
    while let Some(row) = rows.next().map_err(|_| rpg_maker_state_error())? {
        cancellation.ensure_running()?;
        let location_raw = sqlite_text_with_cancellation(row, 0, "group_location", cancellation)
            .map_err(|source| {
                invalid_database_sqlite_read_error(source, "读取完整 Group 术语事实失败")
            })?;
        let kind_raw = sqlite_text_with_cancellation(row, 1, "group_kind", cancellation).map_err(
            |source| invalid_database_sqlite_read_error(source, "读取完整 Group 术语事实失败"),
        )?;
        let source_json =
            sqlite_text_with_cancellation(row, 2, "source_content_json", cancellation).map_err(
                |source| invalid_database_sqlite_read_error(source, "读取完整 Group 术语事实失败"),
            )?;
        let kind = TextGroupKind::from_storage_name(&kind_raw).ok_or_else(rpg_maker_state_error)?;
        let source: TextUnitContent =
            parse_json_with_cancellation(&source_json, "完整 Group 术语事实原文", cancellation)?;
        let facts = prepare_current_resource_facts(
            engine,
            kind,
            &source,
            terminology,
            placeholder_service,
            placeholder_rules,
            cancellation,
        )?;
        if facts.term_indices().len() != facts.terminology_dependencies().len() {
            return Err(rpg_maker_state_error());
        }
        let (group_kind, dependencies) = groups
            .entry(location_raw.clone())
            .or_insert_with(|| (kind, BTreeMap::new()));
        if *group_kind != kind {
            return Err(rpg_maker_state_error());
        }
        for (&term_index, dependency) in facts
            .term_indices()
            .iter()
            .zip(facts.terminology_dependencies())
        {
            cancellation.ensure_running()?;
            if let Some(existing) = dependencies.get(&term_index) {
                if existing != dependency {
                    return Err(rpg_maker_state_error());
                }
            } else {
                dependencies.insert(term_index, dependency.clone());
            }
        }
    }
    drop(rows);
    drop(statement);

    let mut completed = HashMap::with_capacity(groups.len());
    for (location, (_, dependencies)) in groups {
        cancellation.ensure_running()?;
        completed.insert(location, dependencies.into_values().collect());
    }
    cancellation.ensure_running()?;
    Ok(completed)
}

fn resource_semantic_error(_source: ResolvedTranslationSemanticError) -> ProjectLuaCallError {
    rpg_maker_state_error()
}

fn placeholder_compilation_error(source: PlaceholderRuleCompilationError) -> ProjectLuaCallError {
    match source {
        PlaceholderRuleCompilationError::StartWorker {
            operation: _,
            source,
        } => ProjectLuaCallError::worker_spawn(source)
            .with_engine(crate::diagnostic::LuaEngine::RpgMaker),
        _ => rpg_maker_state_error(),
    }
}

fn protect_content(
    service: &Pcre2PlaceholderService,
    engine: RpgMakerEngine,
    kind: TextGroupKind,
    content: &TextUnitContent,
    custom: &CompiledPlaceholderRules,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<Vec<AppliedPlaceholder>, ProjectLuaCallError> {
    cancellation.ensure_running()?;
    let (text, line_boundaries) = content_text_and_line_boundaries(content, cancellation)?;
    cancellation.ensure_running()?;
    let protected = service
        .protect_with_line_boundaries_with_cancellation(
            engine,
            kind,
            &text,
            &line_boundaries,
            custom,
            || cancellation.ensure_running(),
        )?
        .map_err(placeholder_protection_error)?;
    cancellation.ensure_running()?;
    Ok(protected.into_parts().1)
}

fn placeholder_protection_error(source: PlaceholderProtectionError) -> ProjectLuaCallError {
    match source {
        PlaceholderProtectionError::StartWorker {
            operation: _,
            source,
        } => ProjectLuaCallError::worker_spawn(source)
            .with_engine(crate::diagnostic::LuaEngine::RpgMaker),
        _ => rpg_maker_call_violation(crate::diagnostic::LuaValueViolation::InvalidTranslation),
    }
}

fn content_text_and_line_boundaries(
    content: &TextUnitContent,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<(String, Vec<usize>), ProjectLuaCallError> {
    cancellation.ensure_running()?;
    match content {
        TextUnitContent::Value(value) => Ok((
            clone_text_with_cancellation(value, cancellation)?,
            Vec::new(),
        )),
        TextUnitContent::Lines(lines) => {
            let mut offsets = Vec::with_capacity(lines.len().saturating_sub(1));
            let total_bytes = lines
                .iter()
                .try_fold(0_usize, |total, line| {
                    cancellation.ensure_running()?;
                    Ok::<_, ProjectLuaCallError>(total.saturating_add(line.len()).saturating_add(1))
                })?
                .saturating_sub(usize::from(!lines.is_empty()));
            let mut text = String::with_capacity(total_bytes);
            let mut cursor = 0_usize;
            for (index, line) in lines.iter().enumerate() {
                cancellation.ensure_running()?;
                append_text_with_cancellation(&mut text, line, cancellation)?;
                cursor = cursor.saturating_add(line.len());
                if index + 1 < lines.len() {
                    offsets.push(cursor);
                    text.push('\n');
                    cursor = cursor.saturating_add(1);
                }
            }
            cancellation.ensure_running()?;
            Ok((text, offsets))
        }
    }
}

#[derive(Clone, Debug)]
struct PlaceholderFact {
    fingerprint: Sha256Fingerprint,
    original: String,
    origin: PlaceholderRuleOrigin,
    label: String,
    scope: String,
    segment: PlaceholderSegment,
}

#[derive(Debug)]
struct CountedPlaceholderFact {
    fact: PlaceholderFact,
    count: usize,
}

#[derive(Debug)]
struct PlaceholderMultiset {
    buckets: HashMap<Sha256Fingerprint, Vec<CountedPlaceholderFact>>,
    total: usize,
}

fn placeholder_multiset(
    bindings: &[AppliedPlaceholder],
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<PlaceholderMultiset, ProjectLuaCallError> {
    let mut buckets = HashMap::<Sha256Fingerprint, Vec<CountedPlaceholderFact>>::new();
    for binding in bindings {
        cancellation.ensure_running()?;
        let fact = placeholder_fact(binding, cancellation)?;
        let bucket = buckets.entry(fact.fingerprint).or_default();
        let mut found = None;
        for (index, existing) in bucket.iter().enumerate() {
            cancellation.ensure_running()?;
            if placeholder_fact_eq_with_cancellation(&existing.fact, &fact, cancellation)? {
                found = Some(index);
                break;
            }
        }
        if let Some(index) = found {
            bucket[index].count += 1;
        } else {
            bucket.push(CountedPlaceholderFact { fact, count: 1 });
        }
    }
    cancellation.ensure_running()?;
    Ok(PlaceholderMultiset {
        buckets,
        total: bindings.len(),
    })
}

impl PlaceholderMultiset {
    fn eq_with_cancellation(
        &self,
        other: &Self,
        cancellation: RpgMakerLuaCancellation<'_>,
    ) -> Result<bool, ProjectLuaCallError> {
        cancellation.ensure_running()?;
        if self.total != other.total || self.buckets.len() != other.buckets.len() {
            return Ok(false);
        }
        for (fingerprint, left_bucket) in &self.buckets {
            cancellation.ensure_running()?;
            let Some(right_bucket) = other.buckets.get(fingerprint) else {
                return Ok(false);
            };
            if left_bucket.len() != right_bucket.len() {
                return Ok(false);
            }
            for left in left_bucket {
                cancellation.ensure_running()?;
                let mut matching_count = None;
                for right in right_bucket {
                    cancellation.ensure_running()?;
                    if placeholder_fact_eq_with_cancellation(&left.fact, &right.fact, cancellation)?
                    {
                        matching_count = Some(right.count);
                        break;
                    }
                }
                if matching_count != Some(left.count) {
                    return Ok(false);
                }
            }
        }
        cancellation.ensure_running()?;
        Ok(true)
    }

    /// Builtin 原字节始终具有控制语义，未改 Current 中的数量必须与原文完全一致。
    /// Custom 原字节也可能来自 NaturalText，脱离 Translate 的 token 来源信息后不能反扫。
    fn builtin_eq_with_cancellation(
        &self,
        translation: &Self,
        cancellation: RpgMakerLuaCancellation<'_>,
    ) -> Result<bool, ProjectLuaCallError> {
        cancellation.ensure_running()?;
        for (fingerprint, source_bucket) in &self.buckets {
            cancellation.ensure_running()?;
            let translation_bucket = translation.buckets.get(fingerprint);
            for source in source_bucket {
                cancellation.ensure_running()?;
                if source.fact.origin != PlaceholderRuleOrigin::BuiltIn {
                    continue;
                }
                let mut translation_count = None;
                if let Some(translation_bucket) = translation_bucket {
                    for candidate in translation_bucket {
                        cancellation.ensure_running()?;
                        if placeholder_fact_eq_with_cancellation(
                            &source.fact,
                            &candidate.fact,
                            cancellation,
                        )? {
                            translation_count = Some(candidate.count);
                            break;
                        }
                    }
                }
                if translation_count != Some(source.count) {
                    return Ok(false);
                }
            }
        }
        for (fingerprint, translation_bucket) in &translation.buckets {
            cancellation.ensure_running()?;
            let source_bucket = self.buckets.get(fingerprint);
            for candidate in translation_bucket {
                cancellation.ensure_running()?;
                if candidate.fact.origin != PlaceholderRuleOrigin::BuiltIn {
                    continue;
                }
                let mut source_count = None;
                if let Some(source_bucket) = source_bucket {
                    for source in source_bucket {
                        cancellation.ensure_running()?;
                        if placeholder_fact_eq_with_cancellation(
                            &source.fact,
                            &candidate.fact,
                            cancellation,
                        )? {
                            source_count = Some(source.count);
                            break;
                        }
                    }
                }
                if source_count != Some(candidate.count) {
                    return Ok(false);
                }
            }
        }
        cancellation.ensure_running()?;
        Ok(true)
    }
}

fn placeholder_fact(
    binding: &AppliedPlaceholder,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<PlaceholderFact, ProjectLuaCallError> {
    cancellation.ensure_running()?;
    let chunk_size = semantic_hash_chunk_size();
    let mut hasher = Sha256FramedHasher::new(b"att.rpg-maker.lua.placeholder-fact");
    hasher.try_frame_chunks(1, binding.original().as_bytes(), chunk_size, || {
        cancellation.ensure_running()
    })?;
    hasher.frame(
        2,
        match binding.origin() {
            PlaceholderRuleOrigin::BuiltIn => b"builtin".as_slice(),
            PlaceholderRuleOrigin::Custom => b"custom".as_slice(),
        },
    );
    hasher.try_frame_chunks(3, binding.label().as_bytes(), chunk_size, || {
        cancellation.ensure_running()
    })?;
    hasher.try_frame_chunks(4, binding.scope().as_bytes(), chunk_size, || {
        cancellation.ensure_running()
    })?;
    hasher.frame(
        5,
        match binding.segment() {
            PlaceholderSegment::Whole => b"whole".as_slice(),
            PlaceholderSegment::Begin => b"begin".as_slice(),
            PlaceholderSegment::End => b"end".as_slice(),
        },
    );
    cancellation.ensure_running()?;
    Ok(PlaceholderFact {
        fingerprint: hasher.finish(),
        original: clone_text_with_cancellation(binding.original(), cancellation)?,
        origin: binding.origin(),
        label: clone_text_with_cancellation(binding.label(), cancellation)?,
        scope: clone_text_with_cancellation(binding.scope(), cancellation)?,
        segment: binding.segment(),
    })
}

fn placeholder_fact_eq_with_cancellation(
    left: &PlaceholderFact,
    right: &PlaceholderFact,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<bool, ProjectLuaCallError> {
    cancellation.ensure_running()?;
    if left.fingerprint != right.fingerprint
        || left.origin != right.origin
        || left.segment != right.segment
    {
        return Ok(false);
    }
    Ok(
        text_eq_with_cancellation(&left.original, &right.original, cancellation)?
            && text_eq_with_cancellation(&left.label, &right.label, cancellation)?
            && text_eq_with_cancellation(&left.scope, &right.scope, cancellation)?,
    )
}

#[derive(Debug)]
struct TerminologyDependencyFact {
    fingerprint: Sha256Fingerprint,
    term: String,
    translation: String,
}

#[derive(Debug)]
struct TerminologyDependencyProof {
    fingerprint: Sha256Fingerprint,
    dependencies: Vec<TerminologyDependencyFact>,
}

impl TerminologyDependencyProof {
    fn from_dependencies(
        dependencies: &[TerminologyDependency],
        cancellation: RpgMakerLuaCancellation<'_>,
    ) -> Result<Self, ProjectLuaCallError> {
        cancellation.ensure_running()?;
        let chunk_size = semantic_hash_chunk_size();
        let mut aggregate = Sha256FramedHasher::new(b"att.rpg-maker.lua.terminology-dependencies");
        let count = u64::try_from(dependencies.len())
            .expect("术语依赖数量必须能表示为 u64")
            .to_le_bytes();
        aggregate.frame(1, &count);
        let mut facts = Vec::with_capacity(dependencies.len());
        for dependency in dependencies {
            cancellation.ensure_running()?;
            let mut fact_hasher =
                Sha256FramedHasher::new(b"att.rpg-maker.lua.terminology-dependency");
            fact_hasher.try_frame_chunks(1, dependency.term().as_bytes(), chunk_size, || {
                cancellation.ensure_running()
            })?;
            fact_hasher.try_frame_chunks(
                2,
                dependency.translation().as_bytes(),
                chunk_size,
                || cancellation.ensure_running(),
            )?;
            let fingerprint = fact_hasher.finish();
            aggregate.frame(2, fingerprint.as_bytes());
            facts.push(TerminologyDependencyFact {
                fingerprint,
                term: clone_text_with_cancellation(dependency.term(), cancellation)?,
                translation: clone_text_with_cancellation(dependency.translation(), cancellation)?,
            });
        }
        cancellation.ensure_running()?;
        Ok(Self {
            fingerprint: aggregate.finish(),
            dependencies: facts,
        })
    }

    fn eq_with_cancellation(
        &self,
        other: &Self,
        cancellation: RpgMakerLuaCancellation<'_>,
    ) -> Result<bool, ProjectLuaCallError> {
        cancellation.ensure_running()?;
        if self.fingerprint != other.fingerprint
            || self.dependencies.len() != other.dependencies.len()
        {
            return Ok(false);
        }
        for (left, right) in self.dependencies.iter().zip(&other.dependencies) {
            cancellation.ensure_running()?;
            if left.fingerprint != right.fingerprint
                || !text_eq_with_cancellation(&left.term, &right.term, cancellation)?
                || !text_eq_with_cancellation(&left.translation, &right.translation, cancellation)?
            {
                return Ok(false);
            }
        }
        cancellation.ensure_running()?;
        Ok(true)
    }
}

fn manual_state_error(_source: ManualTranslationStateError) -> ProjectLuaCallError {
    rpg_maker_state_error()
}

fn with_unit_locator(
    source: ProjectLuaCallError,
    owner: &str,
    group_location: &str,
    unit_role: &str,
) -> ProjectLuaCallError {
    source.with_rpg_maker_locator(owner, group_location, unit_role)
}

fn rpg_maker_lua_cancelled() -> ProjectLuaCallError {
    ProjectLuaCallError::cancelled().with_engine(crate::diagnostic::LuaEngine::RpgMaker)
}

const CANCELLATION_TEXT_CHUNK_BYTES: usize = 64 * 1024;

fn semantic_hash_chunk_size() -> NonZeroUsize {
    NonZeroUsize::new(CANCELLATION_TEXT_CHUNK_BYTES)
        .expect("RPG Maker Lua 语义事实哈希取消检查块大小必须非零")
}

fn unit_key_fingerprint(
    owner: &str,
    group_location: &str,
    unit_role: &str,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<Sha256Fingerprint, ProjectLuaCallError> {
    cancellation.ensure_running()?;
    let chunk_size = semantic_hash_chunk_size();
    let mut hasher = Sha256FramedHasher::new(b"att.rpg-maker.lua.unit-key");
    hasher.try_frame_chunks(1, owner.as_bytes(), chunk_size, || {
        cancellation.ensure_running()
    })?;
    hasher.try_frame_chunks(2, group_location.as_bytes(), chunk_size, || {
        cancellation.ensure_running()
    })?;
    hasher.try_frame_chunks(3, unit_role.as_bytes(), chunk_size, || {
        cancellation.ensure_running()
    })?;
    cancellation.ensure_running()?;
    Ok(hasher.finish())
}

fn group_key_fingerprint(
    owner: RpgMakerAssetOwner,
    location: &str,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<Sha256Fingerprint, ProjectLuaCallError> {
    cancellation.ensure_running()?;
    let chunk_size = semantic_hash_chunk_size();
    let mut hasher = Sha256FramedHasher::new(b"att.rpg-maker.lua.group-key");
    hasher.frame(1, owner.storage_name().as_bytes());
    hasher.try_frame_chunks(2, location.as_bytes(), chunk_size, || {
        cancellation.ensure_running()
    })?;
    cancellation.ensure_running()?;
    Ok(hasher.finish())
}

fn unit_key_eq_with_cancellation(
    left: &RpgMakerUnitKey,
    right: &RpgMakerUnitKey,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<bool, ProjectLuaCallError> {
    if left.fingerprint != right.fingerprint {
        return Ok(false);
    }
    unit_key_parts_eq_with_cancellation(
        left,
        &right.owner,
        &right.group_location,
        &right.unit_role,
        cancellation,
    )
}

fn unit_key_parts_eq_with_cancellation(
    left: &RpgMakerUnitKey,
    owner: &str,
    group_location: &str,
    unit_role: &str,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<bool, ProjectLuaCallError> {
    cancellation.ensure_running()?;
    Ok(text_eq_with_cancellation(&left.owner, owner, cancellation)?
        && text_eq_with_cancellation(&left.group_location, group_location, cancellation)?
        && text_eq_with_cancellation(&left.unit_role, unit_role, cancellation)?)
}

fn text_eq_with_cancellation(
    left: &str,
    right: &str,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<bool, ProjectLuaCallError> {
    cancellation.ensure_running()?;
    if left.len() != right.len() {
        return Ok(false);
    }
    for (left, right) in left
        .as_bytes()
        .chunks(CANCELLATION_TEXT_CHUNK_BYTES)
        .zip(right.as_bytes().chunks(CANCELLATION_TEXT_CHUNK_BYTES))
    {
        cancellation.ensure_running()?;
        if left != right {
            return Ok(false);
        }
    }
    cancellation.ensure_running()?;
    Ok(true)
}

fn clone_text_with_cancellation(
    source: &str,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<String, ProjectLuaCallError> {
    let mut output = String::with_capacity(source.len());
    append_text_with_cancellation(&mut output, source, cancellation)?;
    Ok(output)
}

fn append_text_with_cancellation(
    output: &mut String,
    source: &str,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<(), ProjectLuaCallError> {
    let mut start = 0;
    while start < source.len() {
        cancellation.ensure_running()?;
        let mut end = start
            .saturating_add(CANCELLATION_TEXT_CHUNK_BYTES)
            .min(source.len());
        while !source.is_char_boundary(end) {
            end -= 1;
        }
        output.push_str(&source[start..end]);
        start = end;
    }
    cancellation.ensure_running()
}

enum CancellableSqliteReadError {
    Sqlite(rusqlite::Error),
    Cancelled(ProjectLuaCallError),
}

impl From<rusqlite::Error> for CancellableSqliteReadError {
    fn from(source: rusqlite::Error) -> Self {
        Self::Sqlite(source)
    }
}

impl From<ProjectLuaCallError> for CancellableSqliteReadError {
    fn from(source: ProjectLuaCallError) -> Self {
        Self::Cancelled(source)
    }
}

fn sqlite_value_with_cancellation(
    row: &Row<'_>,
    index: usize,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<SqliteValue, CancellableSqliteReadError> {
    cancellation.ensure_running()?;
    let value = match row.get_ref(index)? {
        ValueRef::Null => SqliteValue::Null,
        ValueRef::Integer(value) => SqliteValue::Integer(value),
        ValueRef::Real(value) => SqliteValue::Real(value),
        ValueRef::Text(bytes) => SqliteValue::Text(clone_sqlite_text_with_cancellation(
            bytes,
            index,
            cancellation,
        )?),
        ValueRef::Blob(bytes) => {
            SqliteValue::Blob(clone_sqlite_blob_with_cancellation(bytes, cancellation)?)
        }
    };
    cancellation.ensure_running()?;
    Ok(value)
}

fn sqlite_text_with_cancellation(
    row: &Row<'_>,
    index: usize,
    column: &'static str,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<String, CancellableSqliteReadError> {
    cancellation.ensure_running()?;
    match row.get_ref(index)? {
        ValueRef::Text(bytes) => clone_sqlite_text_with_cancellation(bytes, index, cancellation),
        value => Err(CancellableSqliteReadError::Sqlite(
            rusqlite::Error::InvalidColumnType(index, column.to_owned(), value.data_type()),
        )),
    }
}

fn sqlite_optional_text_with_cancellation(
    row: &Row<'_>,
    index: usize,
    column: &'static str,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<Option<String>, CancellableSqliteReadError> {
    cancellation.ensure_running()?;
    match row.get_ref(index)? {
        ValueRef::Null => Ok(None),
        ValueRef::Text(bytes) => {
            clone_sqlite_text_with_cancellation(bytes, index, cancellation).map(Some)
        }
        value => Err(CancellableSqliteReadError::Sqlite(
            rusqlite::Error::InvalidColumnType(index, column.to_owned(), value.data_type()),
        )),
    }
}

fn sqlite_blob_with_cancellation(
    row: &Row<'_>,
    index: usize,
    column: &'static str,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<Vec<u8>, CancellableSqliteReadError> {
    cancellation.ensure_running()?;
    match row.get_ref(index)? {
        ValueRef::Blob(bytes) => clone_sqlite_blob_with_cancellation(bytes, cancellation),
        value => Err(CancellableSqliteReadError::Sqlite(
            rusqlite::Error::InvalidColumnType(index, column.to_owned(), value.data_type()),
        )),
    }
}

fn sqlite_optional_blob_with_cancellation(
    row: &Row<'_>,
    index: usize,
    column: &'static str,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<Option<Vec<u8>>, CancellableSqliteReadError> {
    cancellation.ensure_running()?;
    match row.get_ref(index)? {
        ValueRef::Null => Ok(None),
        ValueRef::Blob(bytes) => clone_sqlite_blob_with_cancellation(bytes, cancellation).map(Some),
        value => Err(CancellableSqliteReadError::Sqlite(
            rusqlite::Error::InvalidColumnType(index, column.to_owned(), value.data_type()),
        )),
    }
}

fn clone_sqlite_text_with_cancellation(
    bytes: &[u8],
    index: usize,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<String, CancellableSqliteReadError> {
    cancellation.ensure_running()?;
    let mut text = String::with_capacity(bytes.len());
    let mut pending = Vec::with_capacity(CANCELLATION_TEXT_CHUNK_BYTES + 3);
    for chunk in bytes.chunks(CANCELLATION_TEXT_CHUNK_BYTES) {
        cancellation.ensure_running()?;
        pending.extend_from_slice(chunk);
        match std::str::from_utf8(&pending) {
            Ok(valid) => {
                text.push_str(valid);
                pending.clear();
            }
            Err(source) if source.error_len().is_none() => {
                let valid_up_to = source.valid_up_to();
                let valid = std::str::from_utf8(&pending[..valid_up_to])
                    .expect("Utf8Error::valid_up_to 指向有效 UTF-8 前缀");
                text.push_str(valid);
                pending.copy_within(valid_up_to.., 0);
                pending.truncate(pending.len() - valid_up_to);
            }
            Err(source) => {
                return Err(CancellableSqliteReadError::Sqlite(
                    rusqlite::Error::FromSqlConversionFailure(index, Type::Text, Box::new(source)),
                ));
            }
        }
    }
    if !pending.is_empty() {
        let source = std::str::from_utf8(&pending).expect_err("pending 只保留不完整 UTF-8 后缀");
        return Err(CancellableSqliteReadError::Sqlite(
            rusqlite::Error::FromSqlConversionFailure(index, Type::Text, Box::new(source)),
        ));
    }
    cancellation.ensure_running()?;
    Ok(text)
}

fn clone_sqlite_blob_with_cancellation(
    bytes: &[u8],
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<Vec<u8>, CancellableSqliteReadError> {
    cancellation.ensure_running()?;
    let mut cloned = Vec::with_capacity(bytes.len());
    for chunk in bytes.chunks(CANCELLATION_TEXT_CHUNK_BYTES) {
        cancellation.ensure_running()?;
        cloned.extend_from_slice(chunk);
    }
    cancellation.ensure_running()?;
    Ok(cloned)
}

fn invalid_database_sqlite_read_error(
    source: CancellableSqliteReadError,
    _operation: &'static str,
) -> ProjectLuaCallError {
    match source {
        CancellableSqliteReadError::Sqlite(source) => rpg_maker_sqlite_error(source),
        CancellableSqliteReadError::Cancelled(source) => source,
    }
}

fn lua_sqlite_read_error(
    source: CancellableSqliteReadError,
    _operation: &'static str,
) -> ProjectLuaCallError {
    match source {
        CancellableSqliteReadError::Sqlite(source) => rpg_maker_sqlite_error(source),
        CancellableSqliteReadError::Cancelled(source) => source,
    }
}

fn text_is_blank(
    text: &str,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<bool, ProjectLuaCallError> {
    for (index, character) in text.chars().enumerate() {
        if index % CANCELLATION_TEXT_CHUNK_BYTES == 0 {
            cancellation.ensure_running()?;
        }
        if !character.is_whitespace() {
            return Ok(false);
        }
    }
    cancellation.ensure_running()?;
    Ok(true)
}

fn content_is_blank(
    content: &TextUnitContent,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<bool, ProjectLuaCallError> {
    match content {
        TextUnitContent::Value(value) => text_is_blank(value, cancellation),
        TextUnitContent::Lines(lines) => {
            for line in lines {
                cancellation.ensure_running()?;
                if !text_is_blank(line, cancellation)? {
                    return Ok(false);
                }
            }
            cancellation.ensure_running()?;
            Ok(true)
        }
    }
}

fn content_contains_forbidden(
    content: &TextUnitContent,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<bool, ProjectLuaCallError> {
    let forbidden: &[u8] = match content {
        TextUnitContent::Value(_) => b"\r\0",
        TextUnitContent::Lines(_) => b"\r\n\0",
    };
    let lines = match content {
        TextUnitContent::Value(value) => std::slice::from_ref(value),
        TextUnitContent::Lines(lines) => lines.as_slice(),
    };
    for line in lines {
        for chunk in line.as_bytes().chunks(CANCELLATION_TEXT_CHUNK_BYTES) {
            cancellation.ensure_running()?;
            if chunk.iter().any(|byte| forbidden.contains(byte)) {
                return Ok(true);
            }
        }
    }
    cancellation.ensure_running()?;
    Ok(false)
}

struct CancellableJsonReader<'a> {
    source: &'a [u8],
    position: usize,
    cancellation: RpgMakerLuaCancellation<'a>,
    bytes_until_check: usize,
    cancelled: bool,
}

impl Read for CancellableJsonReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if self.bytes_until_check == 0 {
            if let Err(error) = self.cancellation.ensure_running() {
                self.cancelled = true;
                return Err(io::Error::other(error.message().to_owned()));
            }
            self.bytes_until_check = CANCELLATION_TEXT_CHUNK_BYTES;
        }
        if self.position == self.source.len() {
            return Ok(0);
        }
        let length = output
            .len()
            .min(CANCELLATION_TEXT_CHUNK_BYTES)
            .min(self.bytes_until_check)
            .min(self.source.len() - self.position);
        output[..length].copy_from_slice(&self.source[self.position..self.position + length]);
        self.position += length;
        self.bytes_until_check -= length;
        Ok(length)
    }
}

fn parse_json_with_cancellation<T>(
    source: &str,
    _label: &str,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<T, ProjectLuaCallError>
where
    T: DeserializeOwned,
{
    match deserialize_json_with_cancellation(source, cancellation)? {
        Ok(value) => Ok(value),
        Err(_) => Err(rpg_maker_state_error()),
    }
}

fn deserialize_json_with_cancellation<T>(
    source: &str,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<Result<T, serde_json::Error>, ProjectLuaCallError>
where
    T: DeserializeOwned,
{
    cancellation.ensure_running()?;
    let reader = CancellableJsonReader {
        source: source.as_bytes(),
        position: 0,
        cancellation,
        bytes_until_check: 0,
        cancelled: false,
    };
    // serde_json 的 Read 适配器会逐字节请求输入。BufReader 把底层读取合并为
    // 固定大小的块，使每次取消检查之间既有明确上界，也避免逐字节虚调用。
    let mut reader = BufReader::with_capacity(CANCELLATION_TEXT_CHUNK_BYTES, reader);
    let parsed = serde_json::from_reader(&mut reader);
    if reader.get_ref().cancelled {
        return Err(rpg_maker_lua_cancelled());
    }
    cancellation.ensure_running()?;
    Ok(parsed)
}

struct CancellableJsonWriter<'a> {
    output: Vec<u8>,
    cancellation: RpgMakerLuaCancellation<'a>,
    bytes_until_check: usize,
    cancelled: bool,
}

impl Write for CancellableJsonWriter<'_> {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        if self.bytes_until_check == 0 {
            if let Err(error) = self.cancellation.ensure_running() {
                self.cancelled = true;
                return Err(io::Error::other(error.message().to_owned()));
            }
            self.bytes_until_check = CANCELLATION_TEXT_CHUNK_BYTES;
        }
        let length = input
            .len()
            .min(CANCELLATION_TEXT_CHUNK_BYTES)
            .min(self.bytes_until_check);
        self.output.extend_from_slice(&input[..length]);
        self.bytes_until_check -= length;
        Ok(length)
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Err(error) = self.cancellation.ensure_running() {
            self.cancelled = true;
            return Err(io::Error::other(error.message().to_owned()));
        }
        Ok(())
    }
}

fn encode_json_with_cancellation<T>(
    value: &T,
    _label: &str,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<String, ProjectLuaCallError>
where
    T: Serialize + ?Sized,
{
    cancellation.ensure_running()?;
    let mut writer = CancellableJsonWriter {
        output: Vec::new(),
        cancellation,
        bytes_until_check: 0,
        cancelled: false,
    };
    match serde_json::to_writer(&mut writer, value) {
        Ok(()) => {
            cancellation.ensure_running()?;
            Ok(String::from_utf8(writer.output).expect("serde_json 必须生成有效 UTF-8"))
        }
        Err(_) if writer.cancelled => Err(rpg_maker_lua_cancelled()),
        Err(_) => Err(rpg_maker_state_error()),
    }
}

fn parse_canonical_language(
    value: &str,
    _field: &'static str,
) -> Result<LanguageId, ProjectLuaCallError> {
    let language = LanguageId::parse(value).map_err(|_| rpg_maker_state_error())?;
    if language.as_str() != value {
        return Err(rpg_maker_state_error());
    }
    Ok(language)
}

fn capture_rpg_maker_translation_baseline(
    connection: &Connection,
    engine: RpgMakerEngine,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<RpgMakerTranslationBaseline, ProjectLuaCallError> {
    cancellation.ensure_running()?;
    let mut metadata_statement = connection
        .prepare(
            "SELECT source_language, target_language
             FROM main.metadata",
        )
        .map_err(|_| rpg_maker_state_error())?;
    let mut metadata_rows = metadata_statement
        .query([])
        .map_err(|_| rpg_maker_state_error())?;
    let mut metadata = Vec::new();
    while let Some(row) = metadata_rows.next().map_err(|_| rpg_maker_state_error())? {
        cancellation.ensure_running()?;
        metadata.push((
            sqlite_text_with_cancellation(row, 0, "source_language", cancellation).map_err(
                |source| invalid_database_sqlite_read_error(source, "读取脚本前语言对失败"),
            )?,
            sqlite_text_with_cancellation(row, 1, "target_language", cancellation).map_err(
                |source| invalid_database_sqlite_read_error(source, "读取脚本前语言对失败"),
            )?,
        ));
    }
    drop(metadata_rows);
    drop(metadata_statement);
    if metadata.len() != 1 {
        return Err(rpg_maker_state_error());
    }
    let (source_language, target_language) = metadata.pop().expect("长度已确认");
    cancellation.ensure_running()?;
    let source_language_id =
        parse_canonical_language(&source_language, "metadata.source_language")?;
    let target_language_id =
        parse_canonical_language(&target_language, "metadata.target_language")?;
    let language_pair = LanguagePair::new(source_language_id, target_language_id);

    cancellation.ensure_running()?;
    let mut resource_statement = connection
        .prepare(
            "SELECT resource_kind, canonical_json
             FROM main.rpg_maker_translation_resource
             ORDER BY resource_kind",
        )
        .map_err(|_| rpg_maker_state_error())?;
    let mut resource_query = resource_statement
        .query([])
        .map_err(|_| rpg_maker_state_error())?;
    let mut resource_rows = Vec::new();
    while let Some(row) = resource_query.next().map_err(|_| rpg_maker_state_error())? {
        cancellation.ensure_running()?;
        resource_rows.push((
            sqlite_text_with_cancellation(row, 0, "resource_kind", cancellation).map_err(
                |source| invalid_database_sqlite_read_error(source, "读取脚本前翻译资源失败"),
            )?,
            sqlite_text_with_cancellation(row, 1, "canonical_json", cancellation).map_err(
                |source| invalid_database_sqlite_read_error(source, "读取脚本前翻译资源失败"),
            )?,
        ));
    }
    drop(resource_query);
    drop(resource_statement);
    if resource_rows.len() != 2 {
        return Err(rpg_maker_state_error());
    }
    let placeholder_json = resource_rows
        .iter()
        .find_map(|(kind, json)| (kind == "placeholder_rules").then_some(json))
        .ok_or_else(rpg_maker_state_error)?;
    let terminology_json = resource_rows
        .iter()
        .find_map(|(kind, json)| (kind == "terminology").then_some(json))
        .ok_or_else(rpg_maker_state_error)?;
    let placeholder_json = clone_text_with_cancellation(placeholder_json, cancellation)?;
    let terminology_json = clone_text_with_cancellation(terminology_json, cancellation)?;
    drop(resource_rows);
    cancellation.ensure_running()?;
    let placeholder_service =
        Pcre2PlaceholderService::new_with_cancellation(|| cancellation.ensure_running())?
            .map_err(|_| rpg_maker_state_error())?;
    cancellation.ensure_running()?;
    let placeholder_rules =
        compile_placeholder_rules(&placeholder_service, &placeholder_json, cancellation)?;
    let terminology = compile_terminology_resource(&terminology_json, cancellation)?;
    let group_contexts = load_group_context_fingerprints(connection, None, cancellation)?;
    let group_terminology_dependencies = load_group_terminology_dependencies(
        connection,
        engine,
        terminology.as_ref(),
        &placeholder_service,
        &placeholder_rules,
        cancellation,
    )?;

    cancellation.ensure_running()?;
    let mut current_statement = connection
        .prepare(
            "SELECT unit.owner, text_group.group_location, unit.unit_role,
                    text_group.group_kind, unit.source_content_json,
                    unit.source_context_json, unit.translation_content_json,
                    unit.translation_state
             FROM main.rpg_maker_text_unit AS unit
             JOIN main.rpg_maker_text_group AS text_group
               ON text_group.owner = unit.owner
               AND text_group.group_id = unit.group_id
             WHERE unit.translation_content_json IS NOT NULL
                OR unit.translation_state IS NOT NULL",
        )
        .map_err(|_| rpg_maker_state_error())?;
    let mut current_rows = current_statement
        .query([])
        .map_err(|_| rpg_maker_state_error())?;

    let mut currents = RpgMakerCurrentBaselines::default();
    while let Some(current_row) = current_rows.next().map_err(|_| rpg_maker_state_error())? {
        cancellation.ensure_running()?;
        let owner_raw = sqlite_text_with_cancellation(current_row, 0, "owner", cancellation)
            .map_err(|source| {
                invalid_database_sqlite_read_error(source, "读取脚本前 Current 失败")
            })?;
        let group_location =
            sqlite_text_with_cancellation(current_row, 1, "group_location", cancellation).map_err(
                |source| invalid_database_sqlite_read_error(source, "读取脚本前 Current 失败"),
            )?;
        let unit_role = sqlite_text_with_cancellation(current_row, 2, "unit_role", cancellation)
            .map_err(|source| {
                invalid_database_sqlite_read_error(source, "读取脚本前 Current 失败")
            })?;
        let group_kind = sqlite_text_with_cancellation(current_row, 3, "group_kind", cancellation)
            .map_err(|source| {
                invalid_database_sqlite_read_error(source, "读取脚本前 Current 失败")
            })?;
        let source_content_json =
            sqlite_text_with_cancellation(current_row, 4, "source_content_json", cancellation)
                .map_err(|source| {
                    invalid_database_sqlite_read_error(source, "读取脚本前 Current 失败")
                })?;
        let source_context_json =
            sqlite_text_with_cancellation(current_row, 5, "source_context_json", cancellation)
                .map_err(|source| {
                    invalid_database_sqlite_read_error(source, "读取脚本前 Current 失败")
                })?;
        let translation_content_json = sqlite_optional_text_with_cancellation(
            current_row,
            6,
            "translation_content_json",
            cancellation,
        )
        .map_err(|source| invalid_database_sqlite_read_error(source, "读取脚本前 Current 失败"))?;
        let translation_state = sqlite_optional_blob_with_cancellation(
            current_row,
            7,
            "translation_state",
            cancellation,
        )
        .map_err(|source| invalid_database_sqlite_read_error(source, "读取脚本前 Current 失败"))?;
        let (Some(translation_content_json), Some(translation_state)) =
            (translation_content_json, translation_state)
        else {
            return Err(rpg_maker_state_error());
        };
        cancellation.ensure_running()?;
        let translation_state = Sha256Fingerprint::from_slice(&translation_state)
            .map_err(|_| rpg_maker_state_error())?;
        let owner =
            RpgMakerAssetOwner::from_storage_name(&owner_raw).ok_or_else(rpg_maker_state_error)?;
        let kind =
            TextGroupKind::from_storage_name(&group_kind).ok_or_else(rpg_maker_state_error)?;
        let location =
            RpgMakerLocationCodec::decode(&group_location).map_err(|_| rpg_maker_state_error())?;
        let role = RpgMakerProjectionCodec::decode_role(&unit_role)
            .map_err(|_| rpg_maker_state_error())?;
        cancellation.ensure_running()?;
        let source: TextUnitContent =
            parse_json_with_cancellation(&source_content_json, "脚本前 Unit 原文", cancellation)?;
        let context: serde_json::Value =
            parse_json_with_cancellation(&source_context_json, "脚本前 Unit 上下文", cancellation)?;
        if !context.is_object() {
            return Err(rpg_maker_state_error());
        }
        let resource_facts = prepare_current_resource_facts(
            engine,
            kind,
            &source,
            terminology.as_ref(),
            &placeholder_service,
            &placeholder_rules,
            cancellation,
        )?;
        let identity = TranslationUnitIdentity::new(
            owner,
            kind,
            location,
            role,
            source,
            source_context_json.clone(),
        );
        let group_context = group_contexts
            .get(&group_location)
            .copied()
            .ok_or_else(rpg_maker_state_error)?;
        let manual_state = manual_translation_state_fingerprint_with_cancellation(
            engine,
            &language_pair,
            group_context,
            &identity,
            resource_facts.placeholders(),
            || cancellation.ensure_running(),
        )?
        .map_err(manual_state_error)?;
        let origin = if manual_state == translation_state {
            RpgMakerTranslationOrigin::Manual
        } else {
            let terminology_dependencies = group_terminology_dependencies
                .get(&group_location)
                .ok_or_else(rpg_maker_state_error)?;
            RpgMakerTranslationOrigin::Automatic(TerminologyDependencyProof::from_dependencies(
                terminology_dependencies,
                cancellation,
            )?)
        };
        let baseline = RpgMakerCurrentBaseline {
            group_kind,
            group_context,
            source_content_json,
            source_context_json,
            translation_content_json,
            translation_state,
            placeholders: placeholder_multiset(resource_facts.placeholders(), cancellation)?,
            origin,
        };
        if !currents.insert(owner_raw, group_location, unit_role, baseline, cancellation)? {
            return Err(rpg_maker_state_error());
        }
    }
    drop(current_rows);
    drop(current_statement);

    cancellation.ensure_running()?;
    Ok(RpgMakerTranslationBaseline {
        source_language,
        target_language,
        currents,
        placeholder_cache: RpgMakerPlaceholderCache {
            canonical_json: placeholder_json,
            service: placeholder_service,
            rules: placeholder_rules,
        },
        terminology_cache: RpgMakerTerminologyCache {
            canonical_json: terminology_json,
            terminology,
        },
    })
}

fn validate_rpg_maker_project(
    connection: &Connection,
    expected_project_name: &str,
    engine: RpgMakerEngine,
    translation_baseline: &RpgMakerTranslationBaseline,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<(), ProjectLuaCallError> {
    cancellation.ensure_running()?;
    let resources = validate_metadata_and_resources(
        connection,
        expected_project_name,
        translation_baseline,
        cancellation,
    )?;
    cancellation.ensure_running()?;
    validate_run_plans(connection, cancellation)?;
    cancellation.ensure_running()?;
    validate_assets(
        connection,
        engine,
        &resources,
        translation_baseline,
        cancellation,
    )?;
    cancellation.ensure_running()
}

struct ValidatedRpgMakerResources {
    dialogue_definition_json: String,
    language_pair: LanguagePair,
    terminology: Arc<CompiledTerminology>,
    placeholder_service: Pcre2PlaceholderService,
    placeholder_rules: CompiledPlaceholderRules,
}

fn validate_metadata_and_resources(
    connection: &Connection,
    expected_project_name: &str,
    translation_baseline: &RpgMakerTranslationBaseline,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<ValidatedRpgMakerResources, ProjectLuaCallError> {
    cancellation.ensure_running()?;
    let mut metadata_statement = connection
        .prepare(
            "SELECT name, source_language, target_language,
                    source_snapshot_fingerprint,
                    dialogue_max_fullwidth_chars,
                    scrolling_text_max_fullwidth_chars,
                    help_description_max_fullwidth_chars
             FROM main.metadata",
        )
        .map_err(|_| rpg_maker_state_error())?;
    let mut metadata_rows = metadata_statement
        .query([])
        .map_err(|_| rpg_maker_state_error())?;
    let mut metadata = Vec::new();
    while let Some(row) = metadata_rows.next().map_err(|_| rpg_maker_state_error())? {
        cancellation.ensure_running()?;
        for (index, column) in [
            (4, "dialogue_max_fullwidth_chars"),
            (5, "scrolling_text_max_fullwidth_chars"),
            (6, "help_description_max_fullwidth_chars"),
        ] {
            let value = row.get_ref(index).map_err(|_| rpg_maker_state_error())?;
            max_fullwidth_chars_from_rusqlite_value(value, column)
                .map_err(|_| rpg_maker_state_error())?;
        }
        metadata.push((
            sqlite_text_with_cancellation(row, 0, "name", cancellation).map_err(|source| {
                invalid_database_sqlite_read_error(source, "读取 metadata 失败")
            })?,
            sqlite_text_with_cancellation(row, 1, "source_language", cancellation).map_err(
                |source| invalid_database_sqlite_read_error(source, "读取 metadata 失败"),
            )?,
            sqlite_text_with_cancellation(row, 2, "target_language", cancellation).map_err(
                |source| invalid_database_sqlite_read_error(source, "读取 metadata 失败"),
            )?,
            sqlite_blob_with_cancellation(row, 3, "source_snapshot_fingerprint", cancellation)
                .map_err(|source| {
                    invalid_database_sqlite_read_error(source, "读取 metadata 失败")
                })?,
        ));
    }
    drop(metadata_rows);
    drop(metadata_statement);
    cancellation.ensure_running()?;
    let [(name, source_language, target_language, source_fingerprint)] = metadata.as_slice() else {
        return Err(rpg_maker_state_error());
    };
    name.parse::<ProjectName>()
        .map_err(|_| rpg_maker_state_error())?;
    if name != expected_project_name {
        return Err(rpg_maker_state_error());
    }
    let source_language = parse_canonical_language(source_language, "metadata.source_language")?;
    let target_language = parse_canonical_language(target_language, "metadata.target_language")?;
    Sha256Fingerprint::from_slice(source_fingerprint).map_err(|_| rpg_maker_state_error())?;

    cancellation.ensure_running()?;
    let mut resource_statement = connection
        .prepare(
            "SELECT resource_kind, canonical_json
             FROM main.rpg_maker_translation_resource
             ORDER BY resource_kind",
        )
        .map_err(|_| rpg_maker_state_error())?;
    let mut resource_rows = resource_statement
        .query([])
        .map_err(|_| rpg_maker_state_error())?;
    let mut resources = Vec::new();
    while let Some(row) = resource_rows.next().map_err(|_| rpg_maker_state_error())? {
        cancellation.ensure_running()?;
        resources.push((
            sqlite_text_with_cancellation(row, 0, "resource_kind", cancellation)
                .map_err(|source| invalid_database_sqlite_read_error(source, "读取翻译资源失败"))?,
            sqlite_text_with_cancellation(row, 1, "canonical_json", cancellation)
                .map_err(|source| invalid_database_sqlite_read_error(source, "读取翻译资源失败"))?,
        ));
    }
    drop(resource_rows);
    drop(resource_statement);
    cancellation.ensure_running()?;
    if resources.len() != 2 {
        return Err(rpg_maker_state_error());
    }
    let terminology = resources
        .iter()
        .find_map(|(kind, json)| (kind == "terminology").then_some(json))
        .ok_or_else(rpg_maker_state_error)?;
    let terminology = if text_eq_with_cancellation(
        &translation_baseline.terminology_cache.canonical_json,
        terminology,
        cancellation,
    )? {
        Arc::clone(&translation_baseline.terminology_cache.terminology)
    } else {
        compile_terminology_resource(terminology, cancellation)?
    };
    cancellation.ensure_running()?;
    let placeholder = resources
        .iter()
        .find_map(|(kind, json)| (kind == "placeholder_rules").then_some(json))
        .ok_or_else(rpg_maker_state_error)?;
    let service = translation_baseline.placeholder_cache.service.clone();
    let placeholder_rules = if text_eq_with_cancellation(
        &translation_baseline.placeholder_cache.canonical_json,
        placeholder,
        cancellation,
    )? {
        translation_baseline.placeholder_cache.rules.clone()
    } else {
        compile_placeholder_rules(&service, placeholder, cancellation)?
    };

    cancellation.ensure_running()?;
    let mut definition_statement = connection
        .prepare(
            "SELECT definition_kind, canonical_json
             FROM main.rpg_maker_project_definition",
        )
        .map_err(|_| rpg_maker_state_error())?;
    let mut definition_rows = definition_statement
        .query([])
        .map_err(|_| rpg_maker_state_error())?;
    let mut definitions = Vec::new();
    while let Some(row) = definition_rows
        .next()
        .map_err(|_| rpg_maker_state_error())?
    {
        cancellation.ensure_running()?;
        definitions.push((
            sqlite_text_with_cancellation(row, 0, "definition_kind", cancellation)
                .map_err(|source| invalid_database_sqlite_read_error(source, "读取项目定义失败"))?,
            sqlite_text_with_cancellation(row, 1, "canonical_json", cancellation)
                .map_err(|source| invalid_database_sqlite_read_error(source, "读取项目定义失败"))?,
        ));
    }
    drop(definition_rows);
    drop(definition_statement);
    cancellation.ensure_running()?;
    if definitions.len() != 1 {
        return Err(rpg_maker_state_error());
    }
    let (kind, canonical_json) = definitions.pop().expect("长度已确认");
    if kind != "mv_dialogue_rules" {
        return Err(rpg_maker_state_error());
    }
    validate_mv_dialogue_definition_canonical_json(&canonical_json)
        .map_err(|_| rpg_maker_state_error())?;
    cancellation.ensure_running()?;
    Ok(ValidatedRpgMakerResources {
        dialogue_definition_json: canonical_json,
        language_pair: LanguagePair::new(source_language, target_language),
        terminology,
        placeholder_service: service,
        placeholder_rules,
    })
}

fn validate_run_plans(
    connection: &Connection,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<(), ProjectLuaCallError> {
    cancellation.ensure_running()?;
    let mut statement = connection
        .prepare(SELECT_RUN_PLAN_SINGLETONS)
        .map_err(|_| rpg_maker_state_error())?;
    let mut query = statement.query([]).map_err(|_| rpg_maker_state_error())?;
    let mut rows = Vec::new();
    while let Some(row) = query.next().map_err(|_| rpg_maker_state_error())? {
        cancellation.ensure_running()?;
        let mut values = Vec::with_capacity(5);
        for index in 0..5 {
            values.push(
                sqlite_value_with_cancellation(row, index, cancellation).map_err(|source| {
                    invalid_database_sqlite_read_error(source, "读取运行方案失败")
                })?,
            );
        }
        rows.push(SqliteRow::new(values));
    }
    drop(query);
    drop(statement);
    cancellation.ensure_running()?;
    let plans = decode_project_run_plans(rows).map_err(|_| rpg_maker_state_error())?;
    if plans.init().is_none() {
        return Err(rpg_maker_state_error());
    }
    cancellation.ensure_running()
}

#[derive(Clone)]
struct StoredUnit {
    role_raw: String,
    source_json: String,
    context_json: String,
    semantic_order_key: RpgMakerSemanticOrderKey,
    write_back: RpgMakerWriteBackUnit,
}

struct StoredGroup {
    owner: RpgMakerAssetOwner,
    location_raw: String,
    location: crate::rpg_maker::text::RpgMakerLocation,
    semantic_order_key: RpgMakerSemanticOrderKey,
    kind_raw: String,
    kind: TextGroupKind,
    recipes_raw: String,
    recipes: Vec<TextProjectionRecipe>,
    units: Vec<StoredUnit>,
}

fn validate_assets(
    connection: &Connection,
    engine: RpgMakerEngine,
    resources: &ValidatedRpgMakerResources,
    translation_baseline: &RpgMakerTranslationBaseline,
    cancellation: RpgMakerLuaCancellation<'_>,
) -> Result<(), ProjectLuaCallError> {
    cancellation.ensure_running()?;
    let current_group_contexts = load_group_context_fingerprints(connection, None, cancellation)?;
    let current_group_terminology_dependencies = load_group_terminology_dependencies(
        connection,
        engine,
        resources.terminology.as_ref(),
        &resources.placeholder_service,
        &resources.placeholder_rules,
        cancellation,
    )?;
    let builtin_only_rules = resources
        .placeholder_service
        .compile_custom_with_cancellation(Vec::new(), || cancellation.ensure_running())?
        .map_err(placeholder_compilation_error)?;
    cancellation.ensure_running()?;
    let mut owner_statement = connection
        .prepare(
            "SELECT owner, source_snapshot_fingerprint, asset_snapshot_fingerprint
             FROM main.rpg_maker_asset_owner_state
             ORDER BY CASE owner WHEN 'builtin' THEN 0 WHEN 'rules' THEN 1 END",
        )
        .map_err(|_| rpg_maker_state_error())?;
    let mut owner_rows = owner_statement
        .query([])
        .map_err(|_| rpg_maker_state_error())?;
    let mut owner_fingerprints = HashMap::new();
    while let Some(owner_row) = owner_rows.next().map_err(|_| rpg_maker_state_error())? {
        cancellation.ensure_running()?;
        let owner_raw = sqlite_text_with_cancellation(owner_row, 0, "owner", cancellation)
            .map_err(|source| invalid_database_sqlite_read_error(source, "读取资产 owner 失败"))?;
        let source_fingerprint = sqlite_blob_with_cancellation(
            owner_row,
            1,
            "source_snapshot_fingerprint",
            cancellation,
        )
        .map_err(|source| invalid_database_sqlite_read_error(source, "读取资产 owner 失败"))?;
        let asset_fingerprint =
            sqlite_blob_with_cancellation(owner_row, 2, "asset_snapshot_fingerprint", cancellation)
                .map_err(|source| {
                    invalid_database_sqlite_read_error(source, "读取资产 owner 失败")
                })?;
        let owner =
            RpgMakerAssetOwner::from_storage_name(&owner_raw).ok_or_else(rpg_maker_state_error)?;
        cancellation.ensure_running()?;
        Sha256Fingerprint::from_slice(&source_fingerprint).map_err(|_| rpg_maker_state_error())?;
        let asset = Sha256Fingerprint::from_slice(&asset_fingerprint)
            .map_err(|_| rpg_maker_state_error())?;
        if owner_fingerprints.insert(owner, asset).is_some() {
            return Err(rpg_maker_state_error());
        }
    }
    drop(owner_rows);
    drop(owner_statement);

    cancellation.ensure_running()?;
    let mut group_statement = connection
        .prepare(
            "SELECT owner, group_location, semantic_order_key, group_kind,
                    projection_recipe_json
             FROM main.rpg_maker_text_group
             ORDER BY CASE owner WHEN 'builtin' THEN 0 WHEN 'rules' THEN 1 END,
                      semantic_order_key",
        )
        .map_err(|_| rpg_maker_state_error())?;
    let mut group_rows = group_statement
        .query([])
        .map_err(|_| rpg_maker_state_error())?;
    let mut group_order_keys =
        HashMap::<RpgMakerAssetOwner, HashMap<RpgMakerSemanticOrderKey, String>>::new();
    let mut groups = Vec::new();
    let mut group_indexes = RpgMakerGroupIndexes::default();
    while let Some(group_row) = group_rows.next().map_err(|_| rpg_maker_state_error())? {
        cancellation.ensure_running()?;
        let owner_raw = sqlite_text_with_cancellation(group_row, 0, "owner", cancellation)
            .map_err(|source| {
                invalid_database_sqlite_read_error(source, "读取 RPG Maker Group 失败")
            })?;
        let location_raw =
            sqlite_text_with_cancellation(group_row, 1, "group_location", cancellation).map_err(
                |source| invalid_database_sqlite_read_error(source, "读取 RPG Maker Group 失败"),
            )?;
        let semantic_order_key =
            sqlite_blob_with_cancellation(group_row, 2, "semantic_order_key", cancellation)
                .map_err(|source| {
                    invalid_database_sqlite_read_error(source, "读取 RPG Maker Group 失败")
                })?;
        let kind_raw = sqlite_text_with_cancellation(group_row, 3, "group_kind", cancellation)
            .map_err(|source| {
                invalid_database_sqlite_read_error(source, "读取 RPG Maker Group 失败")
            })?;
        let recipes_raw =
            sqlite_text_with_cancellation(group_row, 4, "projection_recipe_json", cancellation)
                .map_err(|source| {
                    invalid_database_sqlite_read_error(source, "读取 RPG Maker Group 失败")
                })?;
        let owner =
            RpgMakerAssetOwner::from_storage_name(&owner_raw).ok_or_else(rpg_maker_state_error)?;
        if !owner_fingerprints.contains_key(&owner) {
            return Err(rpg_maker_state_error());
        }
        let semantic_order_key = RpgMakerSemanticOrderKey::decode(&semantic_order_key)
            .map_err(|_| rpg_maker_state_error())?;
        if group_order_keys
            .entry(owner)
            .or_default()
            .insert(semantic_order_key.clone(), location_raw.clone())
            .is_some()
        {
            return Err(rpg_maker_state_error());
        }
        cancellation.ensure_running()?;
        let location =
            RpgMakerLocationCodec::decode(&location_raw).map_err(|_| rpg_maker_state_error())?;
        let kind = TextGroupKind::from_storage_name(&kind_raw).ok_or_else(rpg_maker_state_error)?;
        cancellation.ensure_running()?;
        let recipes = RpgMakerProjectionCodec::decode_recipes(&recipes_raw)
            .map_err(|_| rpg_maker_state_error())?;
        cancellation.ensure_running()?;
        let index = groups.len();
        let indexed_location = clone_text_with_cancellation(&location_raw, cancellation)?;
        if !group_indexes.insert(owner, indexed_location, index, cancellation)? {
            return Err(rpg_maker_state_error());
        }
        groups.push(StoredGroup {
            owner,
            location_raw,
            location,
            semantic_order_key,
            kind_raw,
            kind,
            recipes_raw,
            recipes,
            units: Vec::new(),
        });
    }
    drop(group_rows);
    drop(group_statement);

    cancellation.ensure_running()?;
    let mut unit_statement = connection
        .prepare(
            "SELECT unit.owner, text_group.group_location, unit.unit_role, unit.semantic_order_key,
                    unit.source_content_json, unit.source_context_json,
                    unit.translation_content_json, unit.translation_state
             FROM main.rpg_maker_text_unit AS unit
             JOIN main.rpg_maker_text_group AS text_group
               ON text_group.owner = unit.owner
               AND text_group.group_id = unit.group_id
             ORDER BY CASE unit.owner WHEN 'builtin' THEN 0 WHEN 'rules' THEN 1 END,
                      text_group.semantic_order_key, unit.semantic_order_key",
        )
        .map_err(|_| rpg_maker_state_error())?;
    let mut unit_rows = unit_statement
        .query([])
        .map_err(|_| rpg_maker_state_error())?;
    while let Some(unit_row) = unit_rows.next().map_err(|_| rpg_maker_state_error())? {
        cancellation.ensure_running()?;
        let owner_raw = sqlite_text_with_cancellation(unit_row, 0, "owner", cancellation).map_err(
            |source| invalid_database_sqlite_read_error(source, "读取 RPG Maker Unit 失败"),
        )?;
        let location_raw =
            sqlite_text_with_cancellation(unit_row, 1, "group_location", cancellation).map_err(
                |source| invalid_database_sqlite_read_error(source, "读取 RPG Maker Unit 失败"),
            )?;
        let role_raw = sqlite_text_with_cancellation(unit_row, 2, "unit_role", cancellation)
            .map_err(|source| {
                invalid_database_sqlite_read_error(source, "读取 RPG Maker Unit 失败")
            })?;
        let semantic_order_key =
            sqlite_blob_with_cancellation(unit_row, 3, "semantic_order_key", cancellation)
                .map_err(|source| {
                    invalid_database_sqlite_read_error(source, "读取 RPG Maker Unit 失败")
                })?;
        let source_json =
            sqlite_text_with_cancellation(unit_row, 4, "source_content_json", cancellation)
                .map_err(|source| {
                    invalid_database_sqlite_read_error(source, "读取 RPG Maker Unit 失败")
                })?;
        let context_json =
            sqlite_text_with_cancellation(unit_row, 5, "source_context_json", cancellation)
                .map_err(|source| {
                    invalid_database_sqlite_read_error(source, "读取 RPG Maker Unit 失败")
                })?;
        let translation_json = sqlite_optional_text_with_cancellation(
            unit_row,
            6,
            "translation_content_json",
            cancellation,
        )
        .map_err(|source| invalid_database_sqlite_read_error(source, "读取 RPG Maker Unit 失败"))?;
        let translation_state =
            sqlite_optional_blob_with_cancellation(unit_row, 7, "translation_state", cancellation)
                .map_err(|source| {
                    invalid_database_sqlite_read_error(source, "读取 RPG Maker Unit 失败")
                })?;
        let owner =
            RpgMakerAssetOwner::from_storage_name(&owner_raw).ok_or_else(rpg_maker_state_error)?;
        let index = group_indexes
            .get(owner, &location_raw, cancellation)?
            .ok_or_else(rpg_maker_state_error)?;
        let group = &mut groups[index];
        let semantic_order_key = RpgMakerSemanticOrderKey::decode(&semantic_order_key)
            .map_err(|_| rpg_maker_state_error())?;
        if group
            .units
            .iter()
            .any(|unit| unit.semantic_order_key == semantic_order_key)
        {
            return Err(rpg_maker_state_error());
        }
        cancellation.ensure_running()?;
        let role =
            RpgMakerProjectionCodec::decode_role(&role_raw).map_err(|_| rpg_maker_state_error())?;
        let source: TextUnitContent =
            parse_json_with_cancellation(&source_json, "Unit 原文", cancellation).map_err(
                |source| with_unit_locator(source, &owner_raw, &location_raw, &role_raw),
            )?;
        let translation: Option<TextUnitContent> = match translation_json.as_deref() {
            Some(json) => Some(
                parse_json_with_cancellation(json, "Unit 译文", cancellation).map_err(
                    |source| with_unit_locator(source, &owner_raw, &location_raw, &role_raw),
                )?,
            ),
            None => None,
        };
        let translation_state = match translation_state {
            Some(state) => Some(
                Sha256Fingerprint::from_slice(&state)
                    .map_err(|_| rpg_maker_state_error())
                    .map_err(|source| {
                        with_unit_locator(source, &owner_raw, &location_raw, &role_raw)
                    })?,
            ),
            None => None,
        };
        match (translation.as_ref(), translation_state.as_ref()) {
            (None, None) => {}
            (Some(_), Some(_)) => {}
            _ => {
                return Err(with_unit_locator(
                    rpg_maker_state_error(),
                    &owner_raw,
                    &location_raw,
                    &role_raw,
                ));
            }
        }
        let context: serde_json::Value =
            parse_json_with_cancellation(&context_json, "Unit 上下文", cancellation).map_err(
                |source| with_unit_locator(source, &owner_raw, &location_raw, &role_raw),
            )?;
        if !context.is_object() {
            return Err(with_unit_locator(
                rpg_maker_state_error(),
                &owner_raw,
                &location_raw,
                &role_raw,
            ));
        }
        if let (Some(translation), Some(state)) = (translation.as_ref(), translation_state.as_ref())
        {
            let group_context = current_group_contexts
                .get(&location_raw)
                .copied()
                .ok_or_else(|| {
                    with_unit_locator(
                        rpg_maker_state_error(),
                        &owner_raw,
                        &location_raw,
                        &role_raw,
                    )
                })?;
            validate_translation_structure(group.kind, &role, &source, translation, cancellation)
                .map_err(|source| with_unit_locator(source, &owner_raw, &location_raw, &role_raw))?;
            let resource_facts = prepare_current_resource_facts(
                engine,
                group.kind,
                &source,
                resources.terminology.as_ref(),
                &resources.placeholder_service,
                &resources.placeholder_rules,
                cancellation,
            )?;
            let placeholder_facts =
                placeholder_multiset(resource_facts.placeholders(), cancellation)?;
            let translation_json = translation_json
                .as_deref()
                .expect("已解析的译文必须保留原始 JSON");
            let baseline = translation_baseline.currents.get(
                &owner_raw,
                &location_raw,
                &role_raw,
                cancellation,
            )?;
            let existing_current = baseline.filter(|baseline| baseline.translation_state == *state);
            if let Some(baseline) = existing_current {
                let unchanged_semantics = baseline.group_kind == group.kind_raw
                    && baseline.group_context == group_context
                    && text_eq_with_cancellation(
                        &baseline.source_content_json,
                        &source_json,
                        cancellation,
                    )?
                    && text_eq_with_cancellation(
                        &baseline.source_context_json,
                        &context_json,
                        cancellation,
                    )?
                    && resources.language_pair.source().as_str()
                        == translation_baseline.source_language
                    && resources.language_pair.target().as_str()
                        == translation_baseline.target_language
                    && baseline
                        .placeholders
                        .eq_with_cancellation(&placeholder_facts, cancellation)?;
                if !unchanged_semantics {
                    return Err(with_unit_locator(
                        rpg_maker_state_error(),
                        &owner_raw,
                        &location_raw,
                        &role_raw,
                    ));
                }
                let translation_unchanged = text_eq_with_cancellation(
                    &baseline.translation_content_json,
                    translation_json,
                    cancellation,
                )?;
                if translation_unchanged {
                    validate_unchanged_current_placeholders(
                        &resources.placeholder_service,
                        &builtin_only_rules,
                        engine,
                        group.kind,
                        resource_facts.placeholders(),
                        translation,
                        cancellation,
                    )
                    .map_err(|source| {
                        with_unit_locator(source, &owner_raw, &location_raw, &role_raw)
                    })?;
                } else {
                    validate_translation_placeholders(
                        &resources.placeholder_service,
                        &resources.placeholder_rules,
                        engine,
                        group.kind,
                        resource_facts.placeholders(),
                        translation,
                        cancellation,
                    )
                    .map_err(|source| {
                        with_unit_locator(source, &owner_raw, &location_raw, &role_raw)
                    })?;
                }
                if let RpgMakerTranslationOrigin::Automatic(baseline_dependencies) =
                    &baseline.origin
                {
                    let terminology_dependencies = current_group_terminology_dependencies
                        .get(&location_raw)
                        .ok_or_else(|| {
                            with_unit_locator(
                                rpg_maker_state_error(),
                                &owner_raw,
                                &location_raw,
                                &role_raw,
                            )
                        })?;
                    let current_dependencies = TerminologyDependencyProof::from_dependencies(
                        terminology_dependencies,
                        cancellation,
                    )?;
                    if !baseline_dependencies
                        .eq_with_cancellation(&current_dependencies, cancellation)?
                    {
                        return Err(with_unit_locator(
                            rpg_maker_state_error(),
                            &owner_raw,
                            &location_raw,
                            &role_raw,
                        ));
                    }
                }
            } else {
                cancellation.ensure_running()?;
                validate_translation_placeholders(
                    &resources.placeholder_service,
                    &resources.placeholder_rules,
                    engine,
                    group.kind,
                    resource_facts.placeholders(),
                    translation,
                    cancellation,
                )
                .map_err(|source| {
                    with_unit_locator(source, &owner_raw, &location_raw, &role_raw)
                })?;
                let identity = TranslationUnitIdentity::new(
                    owner,
                    group.kind,
                    group.location.clone(),
                    role.clone(),
                    source.clone(),
                    context_json.clone(),
                );
                let manual_state = manual_translation_state_fingerprint_with_cancellation(
                    engine,
                    &resources.language_pair,
                    group_context,
                    &identity,
                    resource_facts.placeholders(),
                    || cancellation.ensure_running(),
                )?
                .map_err(manual_state_error)?;
                cancellation.ensure_running()?;
                if manual_state != *state {
                    return Err(with_unit_locator(
                        rpg_maker_state_error(),
                        &owner_raw,
                        &location_raw,
                        &role_raw,
                    ));
                }
            }
        }
        cancellation.ensure_running()?;
        let write_back = RpgMakerWriteBackUnit::new(role, source, translation)
            .map_err(|_| rpg_maker_state_error())?;
        group.units.push(StoredUnit {
            role_raw,
            source_json,
            context_json,
            semantic_order_key,
            write_back,
        });
    }
    drop(unit_rows);
    drop(unit_statement);

    let mut fingerprint_builders = HashMap::new();
    for owner in owner_fingerprints.keys().copied() {
        cancellation.ensure_running()?;
        fingerprint_builders.insert(
            owner,
            RpgMakerTextSnapshotFingerprintBuilder::new(
                owner,
                (owner == RpgMakerAssetOwner::Builtin)
                    .then_some(resources.dialogue_definition_json.as_str()),
            ),
        );
    }
    for group in &groups {
        cancellation.ensure_running()?;
        fingerprint_builders
            .get_mut(&group.owner)
            .expect("Group owner 已验证")
            .group(
                &group.location_raw,
                &group.semantic_order_key,
                &group.kind_raw,
                &group.recipes_raw,
            );
        cancellation.ensure_running()?;
    }
    for group in &groups {
        cancellation.ensure_running()?;
        for unit in &group.units {
            cancellation.ensure_running()?;
            fingerprint_builders
                .get_mut(&group.owner)
                .expect("Unit owner 已验证")
                .unit(
                    &group.location_raw,
                    &unit.role_raw,
                    &unit.semantic_order_key,
                    &unit.source_json,
                    &unit.context_json,
                );
            cancellation.ensure_running()?;
        }
    }

    let mut logical_claims = HashMap::<RpgMakerAssetOwner, Vec<EncodedMutationClaim>>::new();
    for group in &groups {
        cancellation.ensure_running()?;
        let mut write_back_units = Vec::with_capacity(group.units.len());
        for unit in &group.units {
            cancellation.ensure_running()?;
            write_back_units.push(unit.write_back.clone());
        }
        let mut recipes = Vec::with_capacity(group.recipes.len());
        for recipe in &group.recipes {
            cancellation.ensure_running()?;
            recipes.push(recipe.clone());
        }
        cancellation.ensure_running()?;
        let validated = RpgMakerWriteBackGroup::from_recipes(
            group.kind,
            group.location.clone(),
            write_back_units,
            recipes,
        )
        .map_err(|_| rpg_maker_state_error())?;
        cancellation.ensure_running()?;
        for lock in validated.mutation_claims().locks() {
            cancellation.ensure_running()?;
            let resource_key = RpgMakerProjectionCodec::encode_mutation_resource(lock.resource())
                .map_err(|_| rpg_maker_state_error())?;
            cancellation.ensure_running()?;
            logical_claims
                .entry(group.owner)
                .or_default()
                .push(EncodedMutationClaim::new(
                    resource_key,
                    lock.access(),
                    group.location_raw.clone(),
                    group.semantic_order_key.clone(),
                ));
        }
    }
    for claims in logical_claims.values_mut() {
        cancellation.ensure_running()?;
        sort_logical_claims(claims);
        cancellation.ensure_running()?;
    }

    cancellation.ensure_running()?;
    let mut claim_statement = connection
        .prepare(
            "SELECT claim.owner, text_group.group_location, claim.resource_key, claim.access
             FROM main.rpg_maker_mutation_claim AS claim
             JOIN main.rpg_maker_text_group AS text_group
               ON text_group.owner = claim.owner
              AND text_group.group_id = claim.group_id
             ORDER BY CASE claim.owner WHEN 'builtin' THEN 0 WHEN 'rules' THEN 1 END,
                      claim.resource_key, claim.access, text_group.group_location",
        )
        .map_err(|_| rpg_maker_state_error())?;
    let mut claim_rows = claim_statement
        .query([])
        .map_err(|_| rpg_maker_state_error())?;
    let mut stored_claims = HashMap::<RpgMakerAssetOwner, Vec<EncodedMutationClaim>>::new();
    while let Some(claim_row) = claim_rows.next().map_err(|_| rpg_maker_state_error())? {
        cancellation.ensure_running()?;
        let owner_raw = sqlite_text_with_cancellation(claim_row, 0, "owner", cancellation)
            .map_err(|source| {
                invalid_database_sqlite_read_error(source, "读取 Mutation Claim 失败")
            })?;
        let location_raw =
            sqlite_text_with_cancellation(claim_row, 1, "group_location", cancellation).map_err(
                |source| invalid_database_sqlite_read_error(source, "读取 Mutation Claim 失败"),
            )?;
        let resource_raw =
            sqlite_text_with_cancellation(claim_row, 2, "resource_key", cancellation).map_err(
                |source| invalid_database_sqlite_read_error(source, "读取 Mutation Claim 失败"),
            )?;
        let access_raw = sqlite_text_with_cancellation(claim_row, 3, "access", cancellation)
            .map_err(|source| {
                invalid_database_sqlite_read_error(source, "读取 Mutation Claim 失败")
            })?;
        let owner =
            RpgMakerAssetOwner::from_storage_name(&owner_raw).ok_or_else(rpg_maker_state_error)?;
        let semantic_order_key = group_indexes
            .get(owner, &location_raw, cancellation)?
            .and_then(|index| groups.get(index))
            .map(|group| group.semantic_order_key.clone())
            .ok_or_else(rpg_maker_state_error)?;
        cancellation.ensure_running()?;
        let resource = RpgMakerProjectionCodec::decode_mutation_resource(&resource_raw)
            .map_err(|_| rpg_maker_state_error())?;
        if RpgMakerProjectionCodec::encode_mutation_resource(&resource)
            .map_err(|_| rpg_maker_state_error())?
            != resource_raw
        {
            return Err(rpg_maker_state_error());
        }
        cancellation.ensure_running()?;
        let access = MutationResourceAccess::from_storage_name(&access_raw)
            .ok_or_else(rpg_maker_state_error)?;
        stored_claims
            .entry(owner)
            .or_default()
            .push(EncodedMutationClaim::new(
                resource_raw,
                access,
                location_raw,
                semantic_order_key,
            ));
    }
    drop(claim_rows);
    drop(claim_statement);

    for owner in owner_fingerprints.keys() {
        cancellation.ensure_running()?;
        let logical = logical_claims.remove(owner).unwrap_or_default();
        cancellation.ensure_running()?;
        let expected_summary = collision_summary(&logical).map_err(|_| rpg_maker_state_error())?;
        cancellation.ensure_running()?;
        let actual_summary = stored_claims.remove(owner).unwrap_or_default();
        if actual_summary != expected_summary {
            return Err(rpg_maker_state_error());
        }
        let builder = fingerprint_builders
            .get_mut(owner)
            .expect("每个 active owner 已建立指纹");
        for claim in &logical {
            cancellation.ensure_running()?;
            builder.claim(
                &claim.resource_key,
                claim.access.storage_name(),
                &claim.group_location,
            );
            cancellation.ensure_running()?;
        }
    }
    if !stored_claims.is_empty() || !logical_claims.is_empty() {
        return Err(rpg_maker_state_error());
    }
    for (owner, expected) in owner_fingerprints {
        cancellation.ensure_running()?;
        let actual = fingerprint_builders
            .remove(&owner)
            .expect("每个 active owner 已建立指纹")
            .finish();
        cancellation.ensure_running()?;
        if actual != expected {
            return Err(rpg_maker_state_error());
        }
    }
    cancellation.ensure_running()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tempfile::tempdir;

    use crate::rpg_maker::model::{DirectTextPart, DirectTextRecipe, ScalarFieldKey};
    use crate::rpg_maker::project::{MaxFullwidthChars, RpgMakerWriteBackLayoutProfile};
    use crate::rpg_maker::project_database::{
        NewProject, ProjectDatabaseCreationService, ProjectDatabaseCreator,
        SourceSnapshotFingerprint,
    };
    use crate::rpg_maker::text::{
        RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource, StandardDataFile,
    };
    use crate::runtime::sqlite::{RusqliteStorage, RusqliteStorageConfiguration};

    use super::*;
    use crate::project_lua::{
        ProjectLuaFailure, ProjectLuaProgram, ProjectLuaProject, ProjectLuaRunError,
        ProjectLuaRunRequest, run_project_lua,
    };

    #[test]
    fn placeholder_worker_start_keeps_typed_os_code() {
        let compilation =
            placeholder_compilation_error(PlaceholderRuleCompilationError::StartWorker {
                operation: PlaceholderWorkerOperation::CompileCustomRules,
                source: io::Error::from_raw_os_error(8),
            });
        assert_eq!(compilation.kind(), "worker_spawn");
        assert!(matches!(
            compilation.issue,
            super::super::ProjectLuaCallIssue::WorkerSpawn { ref failure, .. }
                if failure.raw_os_code == Some(8)
        ));

        let protection = placeholder_protection_error(PlaceholderProtectionError::StartWorker {
            operation: PlaceholderWorkerOperation::MatchText,
            source: io::Error::from_raw_os_error(8),
        });
        assert_eq!(protection.kind(), "worker_spawn");
        assert!(matches!(
            protection.issue,
            super::super::ProjectLuaCallIssue::WorkerSpawn { ref failure, .. }
                if failure.raw_os_code == Some(8)
        ));
    }

    struct TestProject {
        _temporary: tempfile::TempDir,
        database_path: PathBuf,
        group_location: String,
        unit_role: String,
    }

    fn create_project() -> TestProject {
        let temporary = tempdir().expect("应建立临时目录");
        let database_path = temporary.path().join("project.db");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("应建立测试 runtime");
        let storage = RusqliteStorage::start(RusqliteStorageConfiguration::production())
            .expect("应启动 SQLite 根");
        let width = MaxFullwidthChars::new(24).expect("测试宽度应有效");
        let project = NewProject::new(
            "game".parse().expect("项目名应有效"),
            LanguagePair::new(
                LanguageId::parse("ja").expect("来源语言应有效"),
                LanguageId::parse("zh-Hans").expect("目标语言应有效"),
            ),
            SourceSnapshotFingerprint::from_bytes([0x5a; 32]),
            RpgMakerWriteBackLayoutProfile::new(width, width, width),
        );
        runtime.block_on(async {
            ProjectDatabaseCreationService::new(storage.clone())
                .create(database_path.clone(), project)
                .await
                .expect("应创建当前 RPG Maker 数据库");
            storage.shutdown().await.expect("应关闭 SQLite 根");
        });
        let (group_location, unit_role) = install_asset(&database_path);
        TestProject {
            _temporary: temporary,
            database_path,
            group_location,
            unit_role,
        }
    }

    fn install_asset(database_path: &Path) -> (String, String) {
        let connection = Connection::open(database_path).expect("应打开项目数据库");
        connection
            .pragma_update(None, "foreign_keys", true)
            .expect("应启用外键");
        let placeholder_json =
            serde_json::to_string(&vec![PlaceholderRuleDefinition::new(None, r"\{[^}]+\}")])
                .expect("应编码 Placeholder");
        connection
            .execute(
                "UPDATE rpg_maker_translation_resource
                 SET canonical_json = ?1
                 WHERE resource_kind = 'placeholder_rules'",
                [&placeholder_json],
            )
            .expect("应更新 Placeholder");

        let location = RpgMakerLocation::value(
            RpgMakerSource::data(StandardDataFile::Actors),
            vec![
                RpgMakerLocationStep::index(1),
                RpgMakerLocationStep::key("name"),
            ],
        );
        let group_location = RpgMakerLocationCodec::encode(&location).expect("应编码位置");
        let role = TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("字段键应有效"));
        let unit_role = RpgMakerProjectionCodec::encode_role(&role).expect("应编码角色");
        let group_semantic_order_key = RpgMakerSemanticOrderKey::from_group_location(&location);
        let unit_semantic_order_key =
            RpgMakerSemanticOrderKey::from_unit_location(&location, &role);
        let source = r"\V[1]こんにちは {hero}";
        let source_content = TextUnitContent::Value(source.to_owned());
        let source_json = serde_json::to_string(&source_content).expect("应编码原文");
        let context_json = "{}";
        let recipes = vec![TextProjectionRecipe::Direct(
            DirectTextRecipe::new(
                location.clone(),
                source,
                vec![DirectTextPart::TextSlot { role: role.clone() }],
            )
            .expect("应建立直接配方"),
        )];
        let recipes_json = RpgMakerProjectionCodec::encode_recipes(&recipes).expect("应编码配方");
        let group = RpgMakerWriteBackGroup::from_recipes(
            TextGroupKind::DatabaseEntry,
            location,
            vec![RpgMakerWriteBackUnit::new(role, source_content, None).expect("应建立写回 Unit")],
            recipes,
        )
        .expect("应建立写回 Group");
        let mut claims = group
            .mutation_claims()
            .locks()
            .iter()
            .map(|lock| {
                EncodedMutationClaim::new(
                    RpgMakerProjectionCodec::encode_mutation_resource(lock.resource())
                        .expect("应编码 Claim"),
                    lock.access(),
                    group_location.clone(),
                    group_semantic_order_key.clone(),
                )
            })
            .collect::<Vec<_>>();
        sort_logical_claims(&mut claims);
        let summary = collision_summary(&claims).expect("应建立 Claim 摘要");
        let mut fingerprint = RpgMakerTextSnapshotFingerprintBuilder::new(
            RpgMakerAssetOwner::Builtin,
            Some(r#"{"rules":[]}"#),
        );
        fingerprint.group(
            &group_location,
            &group_semantic_order_key,
            TextGroupKind::DatabaseEntry.storage_name(),
            &recipes_json,
        );
        fingerprint.unit(
            &group_location,
            &unit_role,
            &unit_semantic_order_key,
            &source_json,
            context_json,
        );
        for claim in &claims {
            fingerprint.claim(
                &claim.resource_key,
                claim.access.storage_name(),
                &claim.group_location,
            );
        }
        let asset_fingerprint = fingerprint.finish();
        let init_source_path = database_path
            .parent()
            .expect("测试数据库应有父目录")
            .as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();

        let transaction = connection.unchecked_transaction().expect("应开始测试事务");
        transaction
            .execute(
                "INSERT INTO init_run_plan (singleton, source_path_utf16)
                 VALUES (1, ?1)",
                [init_source_path],
            )
            .expect("应插入 Init 运行方案");
        transaction
            .execute(
                "INSERT INTO rpg_maker_asset_owner_state
                 (owner, source_snapshot_fingerprint, asset_snapshot_fingerprint)
                 VALUES ('builtin', ?1, ?2)",
                params![
                    [0x5a_u8; 32].as_slice(),
                    asset_fingerprint.as_bytes().as_slice()
                ],
            )
            .expect("应插入 owner");
        transaction
            .execute(
                "INSERT INTO rpg_maker_text_group
                 (owner, group_id, group_location, semantic_order_key, group_kind, projection_recipe_json)
                 VALUES ('builtin', 1, ?1, ?2, 'database_entry', ?3)",
                params![
                    group_location,
                    group_semantic_order_key
                        .encode()
                        .expect("应编码 Group 顺序键"),
                    recipes_json
                ],
            )
            .expect("应插入 Group");
        transaction
            .execute(
                "INSERT INTO rpg_maker_text_unit
                 (owner, group_id, unit_role, semantic_order_key,
                  source_content_json, source_context_json,
                  translation_content_json, translation_state)
                 VALUES ('builtin', 1, ?1, ?2, ?3, ?4, NULL, NULL)",
                params![
                    unit_role,
                    unit_semantic_order_key
                        .encode()
                        .expect("应编码 Unit 顺序键"),
                    source_json,
                    context_json
                ],
            )
            .expect("应插入 Unit");
        for claim in summary {
            transaction
                .execute(
                    "INSERT INTO rpg_maker_mutation_claim
                     (owner, group_id, resource_key, access)
                     VALUES ('builtin', 1, ?1, ?2)",
                    params![claim.resource_key, claim.access.storage_name()],
                )
                .expect("应插入 Claim");
        }
        transaction.commit().expect("应提交测试资产");
        (group_location, unit_role)
    }

    fn install_rules_sibling(project: &TestProject) -> String {
        let connection = Connection::open(&project.database_path).expect("应打开项目数据库");
        connection
            .pragma_update(None, "foreign_keys", true)
            .expect("应启用外键");
        let group_order_blob: Vec<u8> = connection
            .query_row(
                "SELECT semantic_order_key
                 FROM rpg_maker_text_group
                 WHERE owner = 'builtin' AND group_location = ?1",
                [&project.group_location],
                |row| row.get(0),
            )
            .expect("应读取 Builtin Group 顺序键");
        let group_order = RpgMakerSemanticOrderKey::decode(&group_order_blob)
            .expect("Builtin Group 顺序键应有效");
        let logical_group_location =
            RpgMakerLocationCodec::decode(&project.group_location).expect("测试 Group 位置应有效");
        let sibling_location = RpgMakerLocation::value(
            RpgMakerSource::data(StandardDataFile::Actors),
            vec![
                RpgMakerLocationStep::index(1),
                RpgMakerLocationStep::key("description"),
            ],
        );
        let sibling_role =
            TextUnitRole::Scalar(ScalarFieldKey::new("description").expect("测试字段键应有效"));
        let sibling_role_raw =
            RpgMakerProjectionCodec::encode_role(&sibling_role).expect("应编码兄弟角色");
        let sibling_order =
            RpgMakerSemanticOrderKey::from_unit_location(&sibling_location, &sibling_role);
        let sibling_source = TextUnitContent::Value("兄弟こんにちは".to_owned());
        let sibling_source_json = serde_json::to_string(&sibling_source).expect("应编码兄弟原文");
        let recipes = vec![TextProjectionRecipe::Direct(
            DirectTextRecipe::new(
                sibling_location,
                "兄弟こんにちは",
                vec![DirectTextPart::TextSlot {
                    role: sibling_role.clone(),
                }],
            )
            .expect("应建立兄弟直接配方"),
        )];
        let recipes_json = RpgMakerProjectionCodec::encode_recipes(&recipes).expect("应编码配方");
        let write_back_group = RpgMakerWriteBackGroup::from_recipes(
            TextGroupKind::DatabaseEntry,
            logical_group_location,
            vec![
                RpgMakerWriteBackUnit::new(sibling_role, sibling_source, None)
                    .expect("应建立兄弟写回 Unit"),
            ],
            recipes,
        )
        .expect("应建立兄弟写回 Group");
        let mut claims = write_back_group
            .mutation_claims()
            .locks()
            .iter()
            .map(|lock| {
                EncodedMutationClaim::new(
                    RpgMakerProjectionCodec::encode_mutation_resource(lock.resource())
                        .expect("应编码兄弟 Claim"),
                    lock.access(),
                    project.group_location.clone(),
                    group_order.clone(),
                )
            })
            .collect::<Vec<_>>();
        sort_logical_claims(&mut claims);
        let summary = collision_summary(&claims).expect("应建立兄弟 Claim 摘要");
        let mut fingerprint =
            RpgMakerTextSnapshotFingerprintBuilder::new(RpgMakerAssetOwner::Rules, None);
        fingerprint.group(
            &project.group_location,
            &group_order,
            TextGroupKind::DatabaseEntry.storage_name(),
            &recipes_json,
        );
        fingerprint.unit(
            &project.group_location,
            &sibling_role_raw,
            &sibling_order,
            &sibling_source_json,
            "{}",
        );
        for claim in &claims {
            fingerprint.claim(
                &claim.resource_key,
                claim.access.storage_name(),
                &claim.group_location,
            );
        }
        let asset_fingerprint = fingerprint.finish();

        let transaction = connection.unchecked_transaction().expect("应开始测试事务");
        transaction
            .execute(
                "INSERT INTO rpg_maker_asset_owner_state
                 (owner, source_snapshot_fingerprint, asset_snapshot_fingerprint)
                 VALUES ('rules', ?1, ?2)",
                params![
                    [0x5a_u8; 32].as_slice(),
                    asset_fingerprint.as_bytes().as_slice()
                ],
            )
            .expect("应插入 Rules owner");
        transaction
            .execute(
                "INSERT INTO rpg_maker_text_group
                 (owner, group_id, group_location, semantic_order_key, group_kind, projection_recipe_json)
                 VALUES ('rules', 1, ?1, ?2, 'database_entry', ?3)",
                params![project.group_location, group_order_blob, recipes_json],
            )
            .expect("应插入 Rules Group");
        transaction
            .execute(
                "INSERT INTO rpg_maker_text_unit
                 (owner, group_id, unit_role, semantic_order_key,
                  source_content_json, source_context_json,
                  translation_content_json, translation_state)
                 VALUES ('rules', 1, ?1, ?2, ?3, '{}', NULL, NULL)",
                params![
                    sibling_role_raw,
                    sibling_order.encode().expect("应编码兄弟顺序键"),
                    sibling_source_json
                ],
            )
            .expect("应插入 Rules Unit");
        for claim in summary {
            transaction
                .execute(
                    "INSERT INTO rpg_maker_mutation_claim
                     (owner, group_id, resource_key, access)
                     VALUES ('rules', 1, ?1, ?2)",
                    params![claim.resource_key, claim.access.storage_name()],
                )
                .expect("应插入 Rules Claim");
        }
        transaction.commit().expect("应提交兄弟资产");
        sibling_role_raw
    }

    fn locator(project: &TestProject) -> String {
        format!(
            "{{owner = \"builtin\", group_location = [=[{}]=], unit_role = [=[{}]=]}}",
            project.group_location, project.unit_role
        )
    }

    fn terminology_json(entries: Vec<TerminologyEntry>) -> String {
        serde_json::to_string(&entries).expect("应编码测试术语")
    }

    fn placeholder_json(definitions: Vec<PlaceholderRuleDefinition>) -> String {
        serde_json::to_string(&definitions).expect("应编码测试 Placeholder")
    }

    fn install_automatic_current(project: &TestProject, terminology: &str) {
        let connection = Connection::open(&project.database_path).expect("应打开项目数据库");
        let translation =
            serde_json::to_string(&TextUnitContent::Value(r"\V[1]你好 {hero}".to_owned()))
                .expect("应编码自动译文");
        connection
            .execute(
                "UPDATE rpg_maker_translation_resource
                 SET canonical_json = ?1
                 WHERE resource_kind = 'terminology'",
                [terminology],
            )
            .expect("应安装脚本前术语");
        connection
            .execute(
                "UPDATE rpg_maker_text_unit
                 SET translation_content_json = ?1, translation_state = ?2
                 WHERE owner = 'builtin'
                   AND group_id = (
                       SELECT group_id FROM rpg_maker_text_group
                       WHERE owner = 'builtin' AND group_location = ?3
                   )
                   AND unit_role = ?4",
                params![
                    translation,
                    [0xa5_u8; 32].as_slice(),
                    project.group_location,
                    project.unit_role
                ],
            )
            .expect("应安装自动 Current");
    }

    fn update_resource_script(kind: &str, canonical_json: &str) -> String {
        format!(
            r#"ctx.db.execute(
  "UPDATE rpg_maker_translation_resource SET canonical_json = ?1 WHERE resource_kind = ?2",
  {{[=[{canonical_json}]=], "{kind}"}}
)"#
        )
    }

    fn stored_resource(project: &TestProject, kind: &str) -> String {
        Connection::open(&project.database_path)
            .expect("应重开项目数据库")
            .query_row(
                "SELECT canonical_json FROM rpg_maker_translation_resource
                 WHERE resource_kind = ?1",
                [kind],
                |row| row.get(0),
            )
            .expect("应读取测试翻译资源")
    }

    fn run(
        project: &TestProject,
        source: &str,
    ) -> Result<super::super::ProjectLuaRunReport, ProjectLuaRunError> {
        let cancellation = ProjectLuaCancellation::default();
        let connection = Connection::open(&project.database_path).expect("应打开项目数据库");
        run_project_lua(
            connection,
            ProjectLuaRunRequest::new(
                ProjectLuaProject::new("game", "mz"),
                ProjectLuaProgram::new("rpg.lua", source.as_bytes(), Vec::new()),
                rpg_maker_project_lua_adapter(RpgMakerEngine::Mz, cancellation.clone()),
            )
            .with_cancellation(cancellation),
        )
    }

    #[derive(Debug)]
    struct CancelAtPhaseProbe {
        token: ProjectLuaCancellation,
        phase: RpgMakerLuaCancellationPhase,
        cancel_at: usize,
        observed: AtomicUsize,
    }

    impl CancelAtPhaseProbe {
        fn new(
            token: ProjectLuaCancellation,
            phase: RpgMakerLuaCancellationPhase,
            cancel_at: usize,
        ) -> Self {
            Self {
                token,
                phase,
                cancel_at,
                observed: AtomicUsize::new(0),
            }
        }

        fn observed(&self) -> usize {
            self.observed.load(Ordering::SeqCst)
        }
    }

    impl RpgMakerLuaCancellationProbe for CancelAtPhaseProbe {
        fn ensure_running(
            &self,
            phase: RpgMakerLuaCancellationPhase,
        ) -> Result<(), ProjectLuaCallError> {
            if phase == self.phase {
                let observed = self.observed.fetch_add(1, Ordering::SeqCst) + 1;
                if observed == self.cancel_at {
                    self.token.cancel();
                }
            }
            if self.token.is_cancelled() {
                Err(rpg_maker_lua_cancelled())
            } else {
                Ok(())
            }
        }
    }

    fn run_with_cancellation_probe(
        project: &TestProject,
        source: &str,
        token: ProjectLuaCancellation,
        probe: Arc<CancelAtPhaseProbe>,
    ) -> Result<super::super::ProjectLuaRunReport, ProjectLuaRunError> {
        let connection = Connection::open(&project.database_path).expect("应打开项目数据库");
        let adapter: Arc<dyn ProjectLuaEngineAdapter> = Arc::new(
            RpgMakerProjectLuaAdapter::with_cancellation_probe(RpgMakerEngine::Mz, probe),
        );
        run_project_lua(
            connection,
            ProjectLuaRunRequest::new(
                ProjectLuaProject::new("game", "mz"),
                ProjectLuaProgram::new("rpg.lua", source.as_bytes(), Vec::new()),
                adapter,
            )
            .with_cancellation(token),
        )
    }

    fn table_exists(database_path: &Path, table: &str) -> bool {
        Connection::open(database_path)
            .expect("应重开项目数据库")
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM sqlite_schema
                     WHERE type = 'table' AND name = ?1
                 )",
                [table],
                |row| row.get(0),
            )
            .expect("应检查测试标记表")
    }

    #[test]
    fn group_context_loader_merges_owners_and_ignores_target_translations() {
        let project = create_project();
        let connection = Connection::open(&project.database_path).expect("应打开项目数据库");
        connection
            .pragma_update(None, "foreign_keys", true)
            .expect("应启用测试连接外键");
        let cancellation_token = ProjectLuaCancellation::default();
        let adapter =
            RpgMakerProjectLuaAdapter::new(RpgMakerEngine::Mz, cancellation_token.clone());
        let cancellation = adapter.cancellation(RpgMakerLuaCancellationPhase::Validation);
        let fingerprint = || {
            load_group_context_fingerprints(
                &connection,
                Some(&project.group_location),
                cancellation,
            )
            .expect("应读取完整 Group 语境")
            .get(&project.group_location)
            .copied()
            .expect("测试 Group 应存在")
        };
        let base = fingerprint();
        let (group_order, recipes): (Vec<u8>, String) = connection
            .query_row(
                "SELECT semantic_order_key, projection_recipe_json
                 FROM rpg_maker_text_group
                 WHERE owner = 'builtin' AND group_location = ?1",
                [&project.group_location],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("应读取现有 Group 定义");
        let sibling_role = RpgMakerProjectionCodec::encode_role(&TextUnitRole::Scalar(
            ScalarFieldKey::new("description").expect("测试字段键应有效"),
        ))
        .expect("应编码兄弟角色");
        let sibling_order = RpgMakerSemanticOrderKey::new(vec![u64::MAX - 1], 7)
            .encode()
            .expect("应编码兄弟顺序键");
        let sibling_source = serde_json::to_string(&TextUnitContent::Value("兄弟原文".to_owned()))
            .expect("应编码兄弟原文");
        connection
            .execute(
                "INSERT INTO rpg_maker_asset_owner_state
                 (owner, source_snapshot_fingerprint, asset_snapshot_fingerprint)
                 VALUES ('rules', ?1, ?2)",
                params![[0x5a_u8; 32].as_slice(), [0x63_u8; 32].as_slice()],
            )
            .expect("应插入 Rules owner");
        connection
            .execute(
                "INSERT INTO rpg_maker_text_group
                 (owner, group_id, group_location, semantic_order_key, group_kind, projection_recipe_json)
                 VALUES ('rules', 1, ?1, ?2, 'database_entry', ?3)",
                params![project.group_location, group_order, recipes],
            )
            .expect("应插入跨 owner 同一 Group");
        connection
            .execute(
                "INSERT INTO rpg_maker_text_unit
                 (owner, group_id, unit_role, semantic_order_key,
                  source_content_json, source_context_json,
                  translation_content_json, translation_state)
                 VALUES ('rules', 1, ?1, ?2, ?3, '{}', NULL, NULL)",
                params![sibling_role, sibling_order, sibling_source],
            )
            .expect("应插入跨 owner 兄弟 Unit");

        let with_rules_sibling = fingerprint();
        assert_ne!(
            with_rules_sibling, base,
            "跨 Builtin/Rules 合并的兄弟 Unit 必须进入完整 Group 语境"
        );
        let sibling_translation =
            serde_json::to_string(&TextUnitContent::Value("兄弟译文".to_owned()))
                .expect("应编码兄弟译文");
        connection
            .execute(
                "UPDATE rpg_maker_text_unit
                 SET translation_content_json = ?1, translation_state = ?2
                 WHERE owner = 'rules'
                   AND group_id = (
                       SELECT group_id FROM rpg_maker_text_group
                       WHERE owner = 'rules' AND group_location = ?3
                   )
                   AND unit_role = ?4",
                params![
                    sibling_translation,
                    [0x81_u8; 32].as_slice(),
                    project.group_location,
                    sibling_role
                ],
            )
            .expect("应更新兄弟目标译文");
        assert_eq!(
            fingerprint(),
            with_rules_sibling,
            "兄弟目标译文和旧状态不能进入完整 Group 语境"
        );
        let changed_source =
            serde_json::to_string(&TextUnitContent::Value("变化后的兄弟原文".to_owned()))
                .expect("应编码变化后的兄弟原文");
        connection
            .execute(
                "UPDATE rpg_maker_text_unit
                 SET source_content_json = ?1
                 WHERE owner = 'rules'
                   AND group_id = (
                       SELECT group_id FROM rpg_maker_text_group
                       WHERE owner = 'rules' AND group_location = ?2
                   )
                   AND unit_role = ?3",
                params![changed_source, project.group_location, sibling_role],
            )
            .expect("应更新兄弟原文");
        assert_ne!(
            fingerprint(),
            with_rules_sibling,
            "跨 owner 兄弟原文变化必须改变完整 Group 语境"
        );
        assert!(!cancellation_token.is_cancelled());
    }

    #[test]
    fn group_terminology_loader_merges_sibling_units_in_file_order() {
        let project = create_project();
        install_rules_sibling(&project);
        let connection = Connection::open(&project.database_path).expect("应打开项目数据库");
        let token = ProjectLuaCancellation::default();
        let adapter = RpgMakerProjectLuaAdapter::new(RpgMakerEngine::Mz, token.clone());
        let cancellation = adapter.cancellation(RpgMakerLuaCancellationPhase::Validation);
        let terminology_json = terminology_json(vec![
            TerminologyEntry::new("兄弟", "Sibling", vec!["兄弟".to_owned()]),
            TerminologyEntry::new("こんにちは", "你好", vec!["こんにちは".to_owned()]),
        ]);
        let terminology =
            compile_terminology_resource(&terminology_json, cancellation).expect("术语应可编译");
        let placeholder_service =
            Pcre2PlaceholderService::new().expect("内置 Placeholder 应可编译");
        let placeholder_rules = compile_placeholder_rules(
            &placeholder_service,
            &stored_resource(&project, "placeholder_rules"),
            cancellation,
        )
        .expect("Placeholder 应可编译");

        let dependencies = load_group_terminology_dependencies(
            &connection,
            RpgMakerEngine::Mz,
            &terminology,
            &placeholder_service,
            &placeholder_rules,
            cancellation,
        )
        .expect("应按完整 Group 汇总术语");

        assert_eq!(
            dependencies.get(&project.group_location),
            Some(&vec![
                TerminologyDependency::new("兄弟", "Sibling"),
                TerminologyDependency::new("こんにちは", "你好"),
            ]),
            "跨 Builtin/Rules 的兄弟 Unit 命中项必须合并，并保持术语文件顺序"
        );
        assert!(!token.is_cancelled());
    }

    #[test]
    fn final_validation_preserves_target_only_changes_and_rolls_back_sibling_source_changes() {
        let project = create_project();
        let sibling_role = install_rules_sibling(&project);
        run(
            &project,
            &format!(
                "ctx.translation.set({}, [=[你好 \\V[1] {{hero}}]=])",
                locator(&project)
            ),
        )
        .expect("应先建立 Builtin Current");
        let sibling_locator = format!(
            "{{owner = \"rules\", group_location = [=[{}]=], unit_role = [=[{}]=]}}",
            project.group_location, sibling_role
        );
        run(
            &project,
            &format!("ctx.translation.set({sibling_locator}, [=[兄弟译文]=])"),
        )
        .expect("应建立跨 owner 兄弟 Current");

        let changed_target =
            serde_json::to_string(&TextUnitContent::Value("兄弟目标修订".to_owned()))
                .expect("应编码兄弟目标修订");
        run(
            &project,
            &format!(
                r#"ctx.db.execute(
                     [=[UPDATE rpg_maker_text_unit SET translation_content_json = ?1
                        WHERE owner = 'rules'
                          AND group_id = (SELECT group_id FROM rpg_maker_text_group
                                          WHERE owner = 'rules' AND group_location = ?2)
                          AND unit_role = ?3]=],
                     {{[=[{changed_target}]=], [=[{}]=], [=[{}]=]}}
                   )"#,
                project.group_location, sibling_role
            ),
        )
        .expect("兄弟目标译文变化不应改变完整 Group 语境");

        let original_source: String = Connection::open(&project.database_path)
            .expect("应重开项目数据库")
            .query_row(
                "SELECT unit.source_content_json
                 FROM rpg_maker_text_unit AS unit
                 JOIN rpg_maker_text_group AS text_group
                   ON text_group.owner = unit.owner
                  AND text_group.group_id = unit.group_id
                 WHERE unit.owner = 'rules'
                   AND text_group.group_location = ?1
                   AND unit.unit_role = ?2",
                params![project.group_location, sibling_role],
                |row| row.get(0),
            )
            .expect("应读取兄弟原文");
        let changed_source =
            serde_json::to_string(&TextUnitContent::Value("兄弟こんばんは".to_owned()))
                .expect("应编码变化后的兄弟原文");
        let error = run(
            &project,
            &format!(
                r#"ctx.db.execute(
                     [=[UPDATE rpg_maker_text_unit SET source_content_json = ?1
                        WHERE owner = 'rules'
                          AND group_id = (SELECT group_id FROM rpg_maker_text_group
                                          WHERE owner = 'rules' AND group_location = ?2)
                          AND unit_role = ?3]=],
                     {{[=[{changed_source}]=], [=[{}]=], [=[{}]=]}}
                   )"#,
                project.group_location, sibling_role
            ),
        )
        .expect_err("兄弟原文变化后不得保留原 Current 状态");
        let ProjectLuaRunError::RolledBack(ProjectLuaFailure::Host(host_error)) = error else {
            panic!("完整 Group 语境失效应由翻译最终校验拒绝");
        };
        assert_eq!(host_error.operation(), Some("translation.validate"));
        assert!(matches!(
            host_error.issue,
            super::super::ProjectLuaCallIssue::Violation(
                crate::diagnostic::LuaValueViolation::StateMismatch
            )
        ));
        assert!(matches!(
            host_error.locator,
            Some(crate::diagnostic::LuaLocator::RpgMaker {
                group_location: Some(ref group_location),
                ..
            }) if group_location.as_str() == project.group_location
        ));
        let stored_source: String = Connection::open(&project.database_path)
            .expect("应重开项目数据库")
            .query_row(
                "SELECT unit.source_content_json
                 FROM rpg_maker_text_unit AS unit
                 JOIN rpg_maker_text_group AS text_group
                   ON text_group.owner = unit.owner
                  AND text_group.group_id = unit.group_id
                 WHERE unit.owner = 'rules'
                   AND text_group.group_location = ?1
                   AND unit.unit_role = ?2",
                params![project.group_location, sibling_role],
                |row| row.get(0),
            )
            .expect("应读取回滚后的兄弟原文");
        assert_eq!(stored_source, original_source, "失败事务必须回滚兄弟原文");
    }

    #[test]
    fn capture_cancellation_stops_before_script_and_rolls_back() {
        let project = create_project();
        run(
            &project,
            &format!(
                "ctx.translation.set({}, [=[你好 \\V[1] {{hero}}]=])",
                locator(&project)
            ),
        )
        .expect("应先建立一项 Current 供捕获阶段读取");

        let token = ProjectLuaCancellation::default();
        let probe = Arc::new(CancelAtPhaseProbe::new(
            token.clone(),
            RpgMakerLuaCancellationPhase::Capture,
            20,
        ));
        let error = run_with_cancellation_probe(
            &project,
            "ctx.db.execute(\"CREATE TABLE lua_capture_marker (value TEXT)\")",
            token,
            Arc::clone(&probe),
        )
        .expect_err("捕获阶段取消必须终止执行");

        assert!(matches!(
            error,
            ProjectLuaRunError::RolledBack(ProjectLuaFailure::Cancelled)
        ));
        assert_eq!(probe.observed(), 20);
        assert!(!table_exists(&project.database_path, "lua_capture_marker"));
    }

    #[test]
    fn final_validation_cancellation_rolls_back_script_changes() {
        let project = create_project();
        let token = ProjectLuaCancellation::default();
        let probe = Arc::new(CancelAtPhaseProbe::new(
            token.clone(),
            RpgMakerLuaCancellationPhase::Validation,
            70,
        ));
        let error = run_with_cancellation_probe(
            &project,
            "ctx.db.execute(\"CREATE TABLE lua_validation_marker (value TEXT)\")",
            token,
            Arc::clone(&probe),
        )
        .expect_err("提交前校验阶段取消必须终止执行");

        assert!(matches!(
            error,
            ProjectLuaRunError::RolledBack(ProjectLuaFailure::Cancelled)
        ));
        assert_eq!(probe.observed(), 70);
        assert!(!table_exists(
            &project.database_path,
            "lua_validation_marker"
        ));
    }

    #[test]
    fn typed_set_direct_revision_and_clear_preserve_exact_current_state() {
        let project = create_project();
        run(
            &project,
            &format!(
                "ctx.translation.set({}, [=[你好 \\V[1] {{hero}}]=])",
                locator(&project)
            ),
        )
        .expect("typed set 应成功");
        let connection = Connection::open(&project.database_path).expect("应重开数据库");
        let (translation, state): (String, Vec<u8>) = connection
            .query_row(
                "SELECT unit.translation_content_json, unit.translation_state
                 FROM rpg_maker_text_unit AS unit
                 JOIN rpg_maker_text_group AS text_group
                   ON text_group.owner = unit.owner
                  AND text_group.group_id = unit.group_id
                 WHERE unit.owner = 'builtin'
                   AND text_group.group_location = ?1
                   AND unit.unit_role = ?2",
                params![project.group_location, project.unit_role],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("应读取人工译文");
        assert_eq!(translation, r#""你好 \\V[1] {hero}""#);
        assert_eq!(state.len(), 32);
        drop(connection);

        run(
            &project,
            &format!(
                r#"ctx.db.execute(
                     [=[UPDATE rpg_maker_text_unit SET translation_content_json = ?1
                        WHERE owner = 'builtin'
                          AND group_id = (SELECT group_id FROM rpg_maker_text_group
                                          WHERE owner = 'builtin' AND group_location = ?2)
                          AND unit_role = ?3]=],
                     {{[=["人工修订 \\V[1] {{hero}}"]=], [=[{}]=], [=[{}]=]}}
                   )"#,
                project.group_location, project.unit_role
            ),
        )
        .expect("直接 SQL 修订已有译文应保留 Current state");
        let connection = Connection::open(&project.database_path).expect("应重开数据库");
        let (translation_after, state_after): (String, Vec<u8>) = connection
            .query_row(
                "SELECT unit.translation_content_json, unit.translation_state
                 FROM rpg_maker_text_unit AS unit
                 JOIN rpg_maker_text_group AS text_group
                   ON text_group.owner = unit.owner
                  AND text_group.group_id = unit.group_id
                 WHERE unit.owner = 'builtin'
                   AND text_group.group_location = ?1
                   AND unit.unit_role = ?2",
                params![project.group_location, project.unit_role],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("应读取修订结果");
        assert_eq!(translation_after, r#""人工修订 \\V[1] {hero}""#);
        assert_eq!(state_after, state);
        drop(connection);

        let state_error = run(
            &project,
            &format!(
                r#"ctx.db.execute(
                     [=[UPDATE main.rpg_maker_text_unit SET translation_state = ?1
                        WHERE owner = 'builtin'
                          AND group_id = (SELECT group_id FROM main.rpg_maker_text_group
                                          WHERE owner = 'builtin' AND group_location = ?2)
                          AND unit_role = ?3]=],
                     {{ctx.db.blob(string.rep("x", 32)), [=[{}]=], [=[{}]=]}}
                   )"#,
                project.group_location, project.unit_role
            ),
        )
        .expect_err("直接 SQL 不能伪造新的 Current 状态");
        let ProjectLuaRunError::RolledBack(ProjectLuaFailure::Host(host_error)) = state_error
        else {
            panic!("伪造 Current 状态必须返回带 locator 的最终校验错误");
        };
        assert_eq!(host_error.operation(), Some("translation.validate"));
        assert!(matches!(
            host_error.locator,
            Some(crate::diagnostic::LuaLocator::RpgMaker {
                owner: Some(ref owner),
                group_location: Some(ref group_location),
                unit_role: Some(ref unit_role),
            }) if owner.as_str() == "builtin"
                && group_location.as_str() == project.group_location
                && unit_role.as_str() == project.unit_role
        ));

        run(
            &project,
            &format!("ctx.translation.clear({})", locator(&project)),
        )
        .expect("typed clear 应成功");
        let cleared: Option<String> = Connection::open(&project.database_path)
            .expect("应重开数据库")
            .query_row(
                "SELECT unit.translation_content_json
                 FROM rpg_maker_text_unit AS unit
                 JOIN rpg_maker_text_group AS text_group
                   ON text_group.owner = unit.owner
                  AND text_group.group_id = unit.group_id
                 WHERE unit.owner = 'builtin'
                   AND text_group.group_location = ?1
                   AND unit.unit_role = ?2",
                params![project.group_location, project.unit_role],
                |row| row.get(0),
            )
            .expect("应读取清理结果");
        assert_eq!(cleared, None);
    }

    #[test]
    fn typed_clear_accepts_preexisting_current_with_extra_custom_literal() {
        let project = create_project();
        run(
            &project,
            &format!(
                "ctx.translation.set({}, [=[你好 \\V[1] {{hero}}]=])",
                locator(&project)
            ),
        )
        .expect("测试前应建立合法人工译文");

        // Translate 已验收的 Current 可能在保留 Custom Placeholder 的同时，
        // 把相同字节作为 NaturalText 再写一次；Lua 不得反向扫描并阻止 clear。
        let accepted_translation = serde_json::to_string(&TextUnitContent::Value(
            r"你好 \V[1] {hero}，正文再次提到 {hero}".to_owned(),
        ))
        .expect("应编码包含额外 Custom 原片段的译文");
        Connection::open(&project.database_path)
            .expect("应重开数据库")
            .execute(
                "UPDATE rpg_maker_text_unit
                 SET translation_content_json = ?1
                 WHERE owner = 'builtin'
                   AND group_id = (
                       SELECT group_id FROM rpg_maker_text_group
                       WHERE owner = 'builtin' AND group_location = ?2
                   )
                   AND unit_role = ?3",
                params![
                    accepted_translation,
                    project.group_location,
                    project.unit_role
                ],
            )
            .expect("应建立 Translate 可以接受的现有 Current");

        run(&project, "return").expect("未改 Current 的额外 Custom 原片段必须保持合法");

        run(
            &project,
            &format!("ctx.translation.clear({})", locator(&project)),
        )
        .expect("typed clear 应能清除含额外 Custom 原片段的现有译文");
        let cleared: (Option<String>, Option<Vec<u8>>) = Connection::open(&project.database_path)
            .expect("应重开数据库")
            .query_row(
                "SELECT unit.translation_content_json, unit.translation_state
                 FROM rpg_maker_text_unit AS unit
                 JOIN rpg_maker_text_group AS text_group
                   ON text_group.owner = unit.owner
                  AND text_group.group_id = unit.group_id
                 WHERE unit.owner = 'builtin'
                   AND text_group.group_location = ?1
                   AND unit.unit_role = ?2",
                params![project.group_location, project.unit_role],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("应读取清除后的译文状态");
        assert_eq!(cleared, (None, None));
    }

    #[test]
    fn typed_clear_accepts_preexisting_current_with_overlapping_custom_natural_text() {
        let project = create_project();
        let overlapping_rules = serde_json::to_string(&vec![
            PlaceholderRuleDefinition::new(None, r"\{hero\}"),
            PlaceholderRuleDefinition::new(None, r"\{hero\}Z"),
        ])
        .expect("应编码会在恢复后字节上重叠的 Custom Placeholder");
        Connection::open(&project.database_path)
            .expect("应重开数据库")
            .execute(
                "UPDATE rpg_maker_translation_resource
                 SET canonical_json = ?1
                 WHERE resource_kind = 'placeholder_rules'",
                [&overlapping_rules],
            )
            .expect("应安装重叠测试规则");
        run(
            &project,
            &format!(
                "ctx.translation.set({}, [=[你好 \\V[1] {{hero}}]=])",
                locator(&project)
            ),
        )
        .expect("测试前应建立合法人工译文");

        // 源文本只命中 {hero}；Translate 可以在该 token 后生成 NaturalText `Z`。
        // 恢复后的 `{hero}Z` 会同时命中两条 Custom 规则，不能据此否定旧 Current。
        let accepted_translation =
            serde_json::to_string(&TextUnitContent::Value(r"你好 \V[1] {hero}Z".to_owned()))
                .expect("应编码恢复后会产生 Custom 重叠的译文");
        Connection::open(&project.database_path)
            .expect("应重开数据库")
            .execute(
                "UPDATE rpg_maker_text_unit
                 SET translation_content_json = ?1
                 WHERE owner = 'builtin'
                   AND group_id = (
                       SELECT group_id FROM rpg_maker_text_group
                       WHERE owner = 'builtin' AND group_location = ?2
                   )
                   AND unit_role = ?3",
                params![
                    accepted_translation,
                    project.group_location,
                    project.unit_role
                ],
            )
            .expect("应建立 Translate 可以接受的现有 Current");

        run(&project, "return").expect("未改 Current 不得反扫 Custom 并误报重叠");
        run(
            &project,
            &format!("ctx.translation.clear({})", locator(&project)),
        )
        .expect("typed clear 应能清除恢复后 Custom 匹配重叠的现有译文");
    }

    #[test]
    fn unchanged_current_rejects_missing_or_extra_builtin_placeholders_but_clear_repairs_it() {
        for invalid_translation in [r"你好 {hero}", r"你好 \V[1] \V[1] {hero}"] {
            let project = create_project();
            run(
                &project,
                &format!(
                    "ctx.translation.set({}, [=[你好 \\V[1] {{hero}}]=])",
                    locator(&project)
                ),
            )
            .expect("测试前应建立合法人工译文");
            let invalid_translation =
                serde_json::to_string(&TextUnitContent::Value(invalid_translation.to_owned()))
                    .expect("应编码缺失或额外 Builtin 的译文");
            Connection::open(&project.database_path)
                .expect("应重开数据库")
                .execute(
                    "UPDATE rpg_maker_text_unit
                     SET translation_content_json = ?1
                     WHERE owner = 'builtin'
                       AND group_id = (
                           SELECT group_id FROM rpg_maker_text_group
                           WHERE owner = 'builtin' AND group_location = ?2
                       )
                       AND unit_role = ?3",
                    params![
                        invalid_translation,
                        project.group_location,
                        project.unit_role
                    ],
                )
                .expect("应建立需要最终校验拒绝的 Current");

            let error = run(&project, "return")
                .expect_err("未改 Current 仍必须保留所需 Placeholder 并拒绝额外 Builtin");
            let ProjectLuaRunError::RolledBack(ProjectLuaFailure::Host(host_error)) = error else {
                panic!("非法未改 Current 必须返回带 locator 的 Placeholder 错误");
            };
            assert!(matches!(
                host_error.issue,
                super::super::ProjectLuaCallIssue::Violation(
                    crate::diagnostic::LuaValueViolation::InvalidTranslation
                )
            ));
            assert_eq!(host_error.operation(), Some("translation.validate"));
            assert!(matches!(
                host_error.locator,
                Some(crate::diagnostic::LuaLocator::RpgMaker {
                    group_location: Some(ref group_location),
                    unit_role: Some(ref unit_role),
                    ..
                }) if group_location.as_str() == project.group_location
                    && unit_role.as_str() == project.unit_role
            ));

            run(
                &project,
                &format!("ctx.translation.clear({})", locator(&project)),
            )
            .expect("clear 必须能够修复最终校验会拒绝的 Current");
        }
    }

    #[test]
    fn typed_set_rejects_wrong_shape_blank_control_and_placeholder_changes() {
        let project = create_project();
        for source in [
            format!("ctx.translation.set({}, {{\"数组\"}})", locator(&project)),
            format!("ctx.translation.set({}, \"   \")", locator(&project)),
            format!(
                "ctx.translation.set({}, \"缺少 Placeholder\")",
                locator(&project)
            ),
            format!(
                "ctx.translation.set({}, \"译文\" .. string.char(13))",
                locator(&project)
            ),
        ] {
            assert!(matches!(
                run(&project, &source),
                Err(ProjectLuaRunError::RolledBack(ProjectLuaFailure::Host(_)))
            ));
        }
        let forged_state = format!(
            r#"ctx.db.execute(
                 [=[UPDATE main.rpg_maker_text_unit
                    SET translation_content_json = ?1, translation_state = ?2
                    WHERE owner = 'builtin'
                      AND group_id = (SELECT group_id FROM main.rpg_maker_text_group
                                      WHERE owner = 'builtin' AND group_location = ?3)
                      AND unit_role = ?4]=],
                 {{
                   [=["伪造 \\V[1] {{hero}}"]=],
                   ctx.db.blob(string.rep("x", 32)),
                   [=[{}]=],
                   [=[{}]=]
                 }}
               )"#,
            project.group_location, project.unit_role
        );
        assert!(matches!(
            run(&project, &forged_state),
            Err(ProjectLuaRunError::RolledBack(ProjectLuaFailure::Host(error)))
                if error.operation() == Some("translation.validate")
        ));
        let translation: Option<String> = Connection::open(&project.database_path)
            .expect("应重开数据库")
            .query_row(
                "SELECT unit.translation_content_json
                 FROM rpg_maker_text_unit AS unit
                 JOIN rpg_maker_text_group AS text_group
                   ON text_group.owner = unit.owner
                  AND text_group.group_id = unit.group_id
                 WHERE unit.owner = 'builtin'
                   AND text_group.group_location = ?1
                   AND unit.unit_role = ?2",
                params![project.group_location, project.unit_role],
                |row| row.get(0),
            )
            .expect("应读取未修改结果");
        assert_eq!(translation, None);
    }

    #[test]
    fn protected_schema_and_final_resource_validation_roll_back_invalid_changes() {
        let project = create_project();
        run(
            &project,
            r#"
local ok = pcall(ctx.db.execute, "DROP TABLE rpg_maker_text_unit")
assert(not ok)
ctx.db.execute("CREATE TABLE lua_private (value TEXT)")
"#,
        )
        .expect("受保护 schema 拒绝可捕获，私有表应提交");
        let private_exists: i64 = Connection::open(&project.database_path)
            .expect("应重开数据库")
            .query_row(
                "SELECT count(*) FROM sqlite_schema
                 WHERE type = 'table' AND name = 'lua_private'",
                [],
                |row| row.get(0),
            )
            .expect("应检查私有表");
        assert_eq!(private_exists, 1);

        let error = run(
            &project,
            r#"ctx.db.execute(
                 [=[UPDATE rpg_maker_translation_resource SET canonical_json = '{}'
                    WHERE resource_kind = 'placeholder_rules']=]
               )"#,
        )
        .expect_err("无效 Placeholder 资源不能提交");
        assert!(matches!(
            error,
            ProjectLuaRunError::RolledBack(ProjectLuaFailure::Host(error))
                if error.operation() == Some("translation.validate")
        ));
        let resource: String = Connection::open(&project.database_path)
            .expect("应重开数据库")
            .query_row(
                "SELECT canonical_json FROM rpg_maker_translation_resource
                 WHERE resource_kind = 'placeholder_rules'",
                [],
                |row| row.get(0),
            )
            .expect("应读取回滚后的资源");
        assert_ne!(resource, "{}");
    }

    #[test]
    fn final_validation_reuses_compiled_canonical_mv_dialogue_definition_contract() {
        let project = create_project();
        let original: String = Connection::open(&project.database_path)
            .expect("应重开项目数据库")
            .query_row(
                "SELECT canonical_json FROM rpg_maker_project_definition
                 WHERE definition_kind = 'mv_dialogue_rules'",
                [],
                |row| row.get(0),
            )
            .expect("应读取脚本前 MV 对话定义");
        for invalid in [
            r#"{"rules":[{"pattern":"("}]}"#,
            r#"{"rules":[{"pattern":"(?<name>.+)"}]}"#,
            r#"{"rules":[{"pattern":"(?<speaker>.+)(?<other>.*)"}]}"#,
            r#"{"rules": []}"#,
        ] {
            let error = run(
                &project,
                &format!(
                    r#"ctx.db.execute(
  [=[UPDATE rpg_maker_project_definition SET canonical_json = ?1
     WHERE definition_kind = 'mv_dialogue_rules']=],
  {{[=[{invalid}]=]}}
)"#
                ),
            )
            .expect_err("无效或非规范 MV 对话定义不能提交");
            assert!(
                matches!(
                    &error,
                    ProjectLuaRunError::RolledBack(ProjectLuaFailure::Host(error))
                        if error.operation() == Some("translation.validate")
                ),
                "无效定义应在最终校验阶段回滚，实际为 {error:?}"
            );
            let stored: String = Connection::open(&project.database_path)
                .expect("应重开项目数据库")
                .query_row(
                    "SELECT canonical_json FROM rpg_maker_project_definition
                     WHERE definition_kind = 'mv_dialogue_rules'",
                    [],
                    |row| row.get(0),
                )
                .expect("应读取回滚后的 MV 对话定义");
            assert_eq!(stored, original);
        }

        let connection = Connection::open(&project.database_path).expect("应重开项目数据库");
        connection
            .pragma_update(None, "foreign_keys", true)
            .expect("应启用测试连接外键");
        connection
            .execute("DELETE FROM rpg_maker_asset_owner_state", [])
            .expect("测试应回到尚未 Extract 的有效项目状态");
        drop(connection);
        let valid = r#"{"rules":[{"pattern":"(?<speaker>.+)"}]}"#;
        run(
            &project,
            &format!(
                r#"ctx.db.execute(
  [=[UPDATE rpg_maker_project_definition SET canonical_json = ?1
     WHERE definition_kind = 'mv_dialogue_rules']=],
  {{[=[{valid}]=]}}
)"#
            ),
        )
        .expect("规范且可编译、只含 speaker 命名捕获的定义应可提交");
        let stored: String = Connection::open(&project.database_path)
            .expect("应重开项目数据库")
            .query_row(
                "SELECT canonical_json FROM rpg_maker_project_definition
                 WHERE definition_kind = 'mv_dialogue_rules'",
                [],
                |row| row.get(0),
            )
            .expect("应读取已提交的 MV 对话定义");
        assert_eq!(stored, valid);
    }

    #[test]
    fn final_validation_rejects_layout_width_storage_type_and_u32_overflow() {
        for column in [
            "dialogue_max_fullwidth_chars",
            "scrolling_text_max_fullwidth_chars",
            "help_description_max_fullwidth_chars",
        ] {
            for invalid in ["1.5", "4294967296"] {
                let project = create_project();
                let error = run(
                    &project,
                    &format!("ctx.db.execute(\"UPDATE metadata SET {column} = {invalid}\")"),
                )
                .expect_err("非 INTEGER 或超出 u32 的布局宽度不能提交");
                assert!(matches!(
                    error,
                    ProjectLuaRunError::RolledBack(ProjectLuaFailure::Host(error))
                        if error.operation() == Some("translation.validate")
                ));
                let (value, kind): (i64, String) = Connection::open(&project.database_path)
                    .expect("应重开项目数据库")
                    .query_row(
                        &format!("SELECT {column}, typeof({column}) FROM metadata"),
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .expect("应读取回滚后的布局宽度");
                assert_eq!(value, 24);
                assert_eq!(kind, "integer");
            }
        }

        let project = create_project();
        run(
            &project,
            "ctx.db.execute(\"UPDATE metadata SET dialogue_max_fullwidth_chars = 25\")",
        )
        .expect("有效正 u32 布局宽度应可提交");
        let value: i64 = Connection::open(&project.database_path)
            .expect("应重开项目数据库")
            .query_row(
                "SELECT dialogue_max_fullwidth_chars FROM metadata",
                [],
                |row| row.get(0),
            )
            .expect("应读取已提交布局宽度");
        assert_eq!(value, 25);
    }

    #[test]
    fn final_validation_rejects_unicode_outer_whitespace_in_translate_profile() {
        let project = create_project();
        for profile_expression in ["char(160)", "char(160) || 'primary'"] {
            let error = run(
                &project,
                &format!(
                    "ctx.db.execute(\"INSERT INTO translate_run_plan \
                     (singleton, profile_id) VALUES (1, \" .. \
                     \"{profile_expression})\")"
                ),
            )
            .expect_err("Unicode 首尾空白的 Translate Profile 不能提交");
            assert!(matches!(
                error,
                ProjectLuaRunError::RolledBack(ProjectLuaFailure::Host(error))
                    if error.operation() == Some("translation.validate")
            ));
            let count: i64 = Connection::open(&project.database_path)
                .expect("应重开项目数据库")
                .query_row("SELECT count(*) FROM translate_run_plan", [], |row| {
                    row.get(0)
                })
                .expect("应确认无效 Translate Profile 已回滚");
            assert_eq!(count, 0);
        }

        run(
            &project,
            "ctx.db.execute(\"INSERT INTO translate_run_plan \
             (singleton, profile_id) VALUES (1, 'primary')\")",
        )
        .expect("无首尾空白的 Translate Profile 应可提交");
        let error = run(
            &project,
            "ctx.db.execute(\"UPDATE translate_run_plan \
             SET profile_id = char(160) || 'primary'\")",
        )
        .expect_err("既有 Translate Profile 也不能改成 Unicode 空白前缀");
        assert!(matches!(
            error,
            ProjectLuaRunError::RolledBack(ProjectLuaFailure::Host(error))
                if error.operation() == Some("translation.validate")
        ));
        let profile: String = Connection::open(&project.database_path)
            .expect("应重开项目数据库")
            .query_row("SELECT profile_id FROM translate_run_plan", [], |row| {
                row.get(0)
            })
            .expect("应读取回滚后的 Translate Profile");
        assert_eq!(profile, "primary");
    }

    #[test]
    fn typed_set_recompiles_when_current_placeholder_resource_changes() {
        let project = create_project();
        run(
            &project,
            &format!(
                r#"
ctx.db.execute(
  "UPDATE rpg_maker_translation_resource SET canonical_json = ?1 " ..
  "WHERE resource_kind = 'placeholder_rules'",
  {{"[]"}}
)
ctx.translation.set({}, [=[译文 \V[1]]=])
"#,
                locator(&project)
            ),
        )
        .expect("typed set 应使用脚本当前写入的 Placeholder 规则");

        let connection = Connection::open(&project.database_path).expect("应重开数据库");
        let (resource, translation): (String, String) = connection
            .query_row(
                "SELECT resource.canonical_json, unit.translation_content_json
                 FROM rpg_maker_translation_resource AS resource
                 CROSS JOIN rpg_maker_text_unit AS unit
                 JOIN rpg_maker_text_group AS text_group
                   ON text_group.owner = unit.owner
                  AND text_group.group_id = unit.group_id
                 WHERE resource.resource_kind = 'placeholder_rules'
                   AND unit.owner = 'builtin'
                   AND text_group.group_location = ?1
                   AND unit.unit_role = ?2",
                params![project.group_location, project.unit_role],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("应读取当前规则与译文");
        assert_eq!(resource, "[]");
        assert_eq!(
            translation,
            serde_json::to_string(&TextUnitContent::Value(r"译文 \V[1]".to_owned()))
                .expect("应编码期望译文")
        );
    }

    #[test]
    fn matched_terminology_change_rejects_existing_automatic_state() {
        let project = create_project();
        let initial = terminology_json(vec![TerminologyEntry::new(
            "こんにちは",
            "你好",
            vec!["こんにちは".to_owned()],
        )]);
        install_automatic_current(&project, &initial);
        let changed = terminology_json(vec![TerminologyEntry::new(
            "こんにちは",
            "您好",
            vec!["こんにちは".to_owned()],
        )]);

        let error = run(&project, &update_resource_script("terminology", &changed))
            .expect_err("命中术语变化不能保留旧自动状态");

        assert!(matches!(
            error,
            ProjectLuaRunError::RolledBack(ProjectLuaFailure::Host(error))
                if error.operation() == Some("translation.validate")
        ));
        assert_eq!(stored_resource(&project, "terminology"), initial);
    }

    #[test]
    fn unrelated_terminology_change_preserves_existing_automatic_state() {
        let project = create_project();
        let initial = terminology_json(vec![TerminologyEntry::new(
            "こんにちは",
            "你好",
            vec!["こんにちは".to_owned()],
        )]);
        install_automatic_current(&project, &initial);
        let changed = terminology_json(vec![
            TerminologyEntry::new("こんにちは", "你好", vec!["こんにちは".to_owned()]),
            TerminologyEntry::new("未命中", "unused", vec!["絶対にない".to_owned()]),
        ]);

        run(&project, &update_resource_script("terminology", &changed))
            .expect("无关术语变化不应使自动状态失效");

        assert_eq!(stored_resource(&project, "terminology"), changed);
    }

    #[test]
    fn terminology_change_does_not_invalidate_manual_state() {
        let project = create_project();
        let initial = terminology_json(vec![TerminologyEntry::new(
            "こんにちは",
            "你好",
            vec!["こんにちは".to_owned()],
        )]);
        run(&project, &update_resource_script("terminology", &initial)).expect("应先安装测试术语");
        run(
            &project,
            &format!(
                "ctx.translation.set({}, [=[你好 \\V[1] {{hero}}]=])",
                locator(&project)
            ),
        )
        .expect("应建立人工 Current");
        let changed = terminology_json(vec![TerminologyEntry::new(
            "こんにちは",
            "您好",
            vec!["こんにちは".to_owned()],
        )]);

        run(&project, &update_resource_script("terminology", &changed))
            .expect("人工状态不绑定 Terminology");

        assert_eq!(stored_resource(&project, "terminology"), changed);
    }

    #[test]
    fn placeholder_change_that_exposes_a_term_rejects_automatic_state() {
        let project = create_project();
        let terminology = terminology_json(vec![TerminologyEntry::new(
            "hero",
            "勇者",
            vec!["hero".to_owned()],
        )]);
        install_automatic_current(&project, &terminology);
        let changed_placeholder = placeholder_json(vec![PlaceholderRuleDefinition::new(
            None,
            r"\{(?<text>[^}]+)\}",
        )]);

        let error = run(
            &project,
            &update_resource_script("placeholder_rules", &changed_placeholder),
        )
        .expect_err("Placeholder 暴露新的自然文本术语依赖时不能保留旧自动状态");

        assert!(matches!(
            error,
            ProjectLuaRunError::RolledBack(ProjectLuaFailure::Host(error))
                if error.operation() == Some("translation.validate")
        ));
    }

    #[test]
    fn automatic_current_allows_direct_translation_revision_with_same_state() {
        let project = create_project();
        let terminology = terminology_json(Vec::new());
        install_automatic_current(&project, &terminology);
        let changed_translation =
            serde_json::to_string(&TextUnitContent::Value(r"\V[1]改写 {hero}".to_owned()))
                .expect("应编码改写译文");
        let script = format!(
            r#"ctx.db.execute(
  "UPDATE rpg_maker_text_unit SET translation_content_json = ?1 " ..
  "WHERE owner = 'builtin' " ..
  "AND group_id = (SELECT group_id FROM rpg_maker_text_group " ..
  "                WHERE owner = 'builtin' AND group_location = ?2) " ..
  "AND unit_role = ?3",
  {{[=[{changed_translation}]=], [=[{}]=], [=[{}]=]}}
)"#,
            project.group_location, project.unit_role
        );

        run(&project, &script).expect("可信 Lua 可以精修已有自动 Current 的译文正文");

        let stored: String = Connection::open(&project.database_path)
            .expect("应重开项目数据库")
            .query_row(
                "SELECT unit.translation_content_json
                 FROM rpg_maker_text_unit AS unit
                 JOIN rpg_maker_text_group AS text_group
                   ON text_group.owner = unit.owner
                  AND text_group.group_id = unit.group_id
                 WHERE unit.owner = 'builtin'
                   AND text_group.group_location = ?1
                   AND unit.unit_role = ?2",
                params![project.group_location, project.unit_role],
                |row| row.get(0),
            )
            .expect("应读取精修后的自动译文");
        assert_eq!(stored, changed_translation);
    }

    #[test]
    fn noncanonical_att_schema_is_rejected_before_script_runs() {
        for schema_change in [
            "ALTER TABLE metadata ADD COLUMN unexpected_value TEXT",
            "DROP INDEX rpg_maker_mutation_claim_resource_idx",
        ] {
            let project = create_project();
            Connection::open(&project.database_path)
                .expect("应打开项目数据库")
                .execute_batch(schema_change)
                .expect("应建立非当前 schema");

            let error = run(
                &project,
                "ctx.db.execute(\"CREATE TABLE lua_script_started (value TEXT)\")",
            )
            .expect_err("非当前 ATT schema 必须在脚本执行前被拒绝");
            assert!(matches!(
                &error,
                ProjectLuaRunError::RolledBack(ProjectLuaFailure::DatabasePrerequisite(
                    ProjectLuaDatabasePrerequisiteError::InvalidProjectState {
                        engine: crate::diagnostic::LuaEngine::RpgMaker,
                        violation: crate::diagnostic::LuaValueViolation::StateMismatch,
                    }
                ))
            ));

            let script_started: i64 = Connection::open(&project.database_path)
                .expect("应重开项目数据库")
                .query_row(
                    "SELECT count(*) FROM sqlite_schema
                     WHERE type = 'table' AND name = 'lua_script_started'",
                    [],
                    |row| row.get(0),
                )
                .expect("应检查脚本标记表");
            assert_eq!(script_started, 0);
        }
    }

    #[test]
    fn final_validation_preserves_selected_project_identity_and_init_run_plan() {
        let project = create_project();

        let name_error = run(
            &project,
            r#"ctx.db.execute("UPDATE main.metadata SET name = 'other-project'")"#,
        )
        .expect_err("不能把项目数据库改成另一个项目");
        assert!(matches!(
            name_error,
            ProjectLuaRunError::RolledBack(ProjectLuaFailure::Host(error))
                if error.operation() == Some("translation.validate")
        ));

        let missing_init_error = run(
            &project,
            r#"ctx.db.execute("DELETE FROM main.init_run_plan")"#,
        )
        .expect_err("不能删除 Init 运行方案");
        assert!(matches!(
            missing_init_error,
            ProjectLuaRunError::RolledBack(ProjectLuaFailure::Host(error))
                if error.operation() == Some("translation.validate")
        ));

        let invalid_init_error = run(
            &project,
            r#"ctx.db.execute(
                 "UPDATE main.init_run_plan SET source_path_utf16 = ?1",
                 {ctx.db.blob(string.char(114, 0))}
               )"#,
        )
        .expect_err("不能把 Init 来源改成相对路径");
        assert!(matches!(
            invalid_init_error,
            ProjectLuaRunError::RolledBack(ProjectLuaFailure::Host(error))
                if error.operation() == Some("translation.validate")
        ));

        let connection = Connection::open(&project.database_path).expect("应重开数据库");
        let name: String = connection
            .query_row("SELECT name FROM metadata", [], |row| row.get(0))
            .expect("应读取项目名");
        let init_count: i64 = connection
            .query_row("SELECT count(*) FROM init_run_plan", [], |row| row.get(0))
            .expect("应检查 Init 运行方案");
        assert_eq!(name, "game");
        assert_eq!(init_count, 1);
    }

    #[test]
    fn line_translation_requires_dense_string_array_and_preserves_strict_slots() {
        let token = ProjectLuaCancellation::default();
        let cancellation = RpgMakerLuaCancellation {
            probe: &token,
            phase: RpgMakerLuaCancellationPhase::ScriptCall,
        };
        let source = TextUnitContent::Lines(vec!["选择".to_owned(), String::new()]);
        assert_eq!(
            parse_translation(
                ProjectLuaValue::Array(vec![
                    ProjectLuaValue::Text("选项".to_owned()),
                    ProjectLuaValue::Text(String::new()),
                ]),
                &source,
                cancellation,
            )
            .expect("稠密字符串数组应可解析"),
            TextUnitContent::Lines(vec!["选项".to_owned(), String::new()])
        );
        assert!(
            parse_translation(
                ProjectLuaValue::Text("选项".to_owned()),
                &source,
                cancellation,
            )
            .is_err()
        );
        let role = TextUnitRole::Choices;
        assert!(
            validate_translation_structure(
                TextGroupKind::EventChoices,
                &role,
                &source,
                &TextUnitContent::Lines(vec!["选项".to_owned(), "新增".to_owned()]),
                cancellation,
            )
            .is_err()
        );
    }

    #[test]
    fn test_database_file_is_not_left_open() {
        let project = create_project();
        fs::remove_file(&project.database_path).expect("Lua 测试不应遗留数据库连接");
    }
}
