//! RPG Maker 项目的原子 Lua 适配器。

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};

use crate::fingerprint::Sha256Fingerprint;
use crate::language::{LanguageId, LanguagePair};
use crate::project_name::ProjectName;
use crate::rpg_maker::RpgMakerEngine;
use crate::rpg_maker::asset::{RpgMakerAssetOwner, RpgMakerTextSnapshotFingerprintBuilder};
use crate::rpg_maker::dialogue::MvDialogueDefinition;
use crate::rpg_maker::location_codec::{RpgMakerLocationCodec, RpgMakerProjectionCodec};
use crate::rpg_maker::model::{
    MutationResourceAccess, TextProjectionRecipe, TextUnitContent, TextUnitContentView,
    TextUnitRole, validate_text_unit_content_structure,
};
use crate::rpg_maker::mutation_claim_summary::{
    EncodedMutationClaim, collision_summary, sort_logical_claims,
};
use crate::rpg_maker::project_database::{ExtractRulesCanonicalJson, decode_init_source_path};
use crate::rpg_maker::text::TextGroupKind;
use crate::rpg_maker::translate::pipeline::{
    AppliedPlaceholder, PlaceholderRuleOrigin, PlaceholderSegment, TranslationUnitIdentity,
};
use crate::rpg_maker::translate::placeholder::{
    CompiledPlaceholderRules, Pcre2PlaceholderService, PlaceholderRuleDefinition,
};
use crate::rpg_maker::translate::semantics::{
    ManualTranslationStateError, manual_translation_state_fingerprint,
};
use crate::rpg_maker::write_back::planner::{RpgMakerWriteBackGroup, RpgMakerWriteBackUnit};
use crate::translation::planning_resource::{TerminologyEntry, compile_terminology};

use super::{
    ProjectLuaCallError, ProjectLuaEngineAdapter, ProjectLuaProject, ProjectLuaSchemaObjectKind,
    ProjectLuaValue,
};

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
    translation_baseline: Mutex<Option<RpgMakerTranslationBaseline>>,
}

impl RpgMakerProjectLuaAdapter {
    pub(crate) fn new(engine: RpgMakerEngine) -> Self {
        Self {
            engine,
            translation_baseline: Mutex::new(None),
        }
    }
}

/// 为命令接线建立 RPG Maker Lua 引擎适配器。
pub(crate) fn rpg_maker_project_lua_adapter(
    engine: RpgMakerEngine,
) -> Arc<dyn ProjectLuaEngineAdapter> {
    Arc::new(RpgMakerProjectLuaAdapter::new(engine))
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
        let locator = parse_locator(locator)?;
        let unit = load_unit(connection, &locator)?;
        let translation = parse_translation(translation, &unit.source_content)?;
        validate_translation_structure(unit.kind, &unit.role, &unit.source_content, &translation)?;
        let placeholders = validate_manual_placeholders(
            self.engine,
            unit.kind,
            &unit.source_content,
            &translation,
            &unit.placeholder_rules_json,
        )?;
        let state = manual_translation_state_fingerprint(
            self.engine,
            &unit.language_pair,
            &unit.identity(),
            &placeholders,
        )
        .map_err(manual_state_error)?;
        let translation_json = serde_json::to_string(&translation).map_err(|source| {
            ProjectLuaCallError::new(
                "invalid_translation",
                format!("无法编码 RPG Maker 人工译文：{source}"),
            )
        })?;
        let changed = connection
            .execute(
                "UPDATE main.rpg_maker_text_unit
                 SET translation_content_json = ?1,
                     translation_state = ?2
                 WHERE owner = ?3 AND group_location = ?4 AND unit_role = ?5",
                params![
                    translation_json,
                    state.as_bytes().as_slice(),
                    locator.owner.storage_name(),
                    locator.group_location,
                    locator.unit_role
                ],
            )
            .map_err(|source| {
                ProjectLuaCallError::new("sqlite", format!("写入 RPG Maker 人工译文失败：{source}"))
            })?;
        if changed != 1 {
            return Err(ProjectLuaCallError::new(
                "unit_not_found",
                "RPG Maker locator 没有命中唯一 Unit",
            ));
        }
        Ok(u64::try_from(changed).expect("受支持平台的 usize 必须能表示为 u64"))
    }

    fn clear_translation(
        &self,
        connection: &Connection,
        locator: ProjectLuaValue,
    ) -> Result<u64, ProjectLuaCallError> {
        let locator = parse_locator(locator)?;
        let changed = connection
            .execute(
                "UPDATE main.rpg_maker_text_unit
                 SET translation_content_json = NULL,
                     translation_state = NULL
                 WHERE owner = ?1 AND group_location = ?2 AND unit_role = ?3",
                params![
                    locator.owner.storage_name(),
                    locator.group_location,
                    locator.unit_role
                ],
            )
            .map_err(|source| {
                ProjectLuaCallError::new("sqlite", format!("清除 RPG Maker 人工译文失败：{source}"))
            })?;
        if changed != 1 {
            return Err(ProjectLuaCallError::new(
                "unit_not_found",
                "RPG Maker locator 没有命中唯一 Unit",
            ));
        }
        Ok(u64::try_from(changed).expect("受支持平台的 usize 必须能表示为 u64"))
    }

    fn capture_database_state(
        &self,
        connection: &Connection,
        _project: &ProjectLuaProject,
    ) -> Result<(), ProjectLuaCallError> {
        let baseline = capture_rpg_maker_translation_baseline(connection, self.engine)?;
        let mut slot = self
            .translation_baseline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_some() {
            return Err(invalid_database("RPG Maker Lua 适配器不能重复执行"));
        }
        *slot = Some(baseline);
        Ok(())
    }

    fn validate_database(
        &self,
        connection: &Connection,
        project: &ProjectLuaProject,
    ) -> Result<(), ProjectLuaCallError> {
        let baseline = self
            .translation_baseline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let baseline = baseline
            .as_ref()
            .ok_or_else(|| invalid_database("缺少 RPG Maker Lua 脚本前译文状态"))?;
        validate_rpg_maker_project(connection, project.name(), self.engine, baseline)
    }
}

type RpgMakerUnitKey = (String, String, String);

#[derive(Debug)]
struct RpgMakerCurrentBaseline {
    group_kind: String,
    source_content_json: String,
    source_context_json: String,
    translation_state: Vec<u8>,
    placeholders: HashMap<PlaceholderFact, usize>,
}

#[derive(Debug)]
struct RpgMakerTranslationBaseline {
    source_language: String,
    target_language: String,
    currents: HashMap<RpgMakerUnitKey, RpgMakerCurrentBaseline>,
}

struct ParsedLocator {
    owner: RpgMakerAssetOwner,
    group_location: String,
    unit_role: String,
}

fn parse_locator(locator: ProjectLuaValue) -> Result<ParsedLocator, ProjectLuaCallError> {
    let ProjectLuaValue::Object(mut fields) = locator else {
        return Err(ProjectLuaCallError::new(
            "invalid_locator",
            "RPG Maker locator 必须是只含 owner、group_location 与 unit_role 的 table",
        ));
    };
    if fields.len() != 3
        || !fields.contains_key("owner")
        || !fields.contains_key("group_location")
        || !fields.contains_key("unit_role")
    {
        return Err(ProjectLuaCallError::new(
            "invalid_locator",
            "RPG Maker locator 必须且只能包含 owner、group_location 与 unit_role",
        ));
    }
    let owner_raw = take_locator_text(&mut fields, "owner")?;
    let owner = RpgMakerAssetOwner::from_storage_name(&owner_raw).ok_or_else(|| {
        ProjectLuaCallError::new(
            "invalid_locator",
            "RPG Maker locator.owner 必须是 builtin 或 rules",
        )
    })?;
    let group_location = take_locator_text(&mut fields, "group_location")?;
    RpgMakerLocationCodec::decode(&group_location).map_err(|source| {
        ProjectLuaCallError::new(
            "invalid_locator",
            format!("RPG Maker locator.group_location 无效：{source}"),
        )
    })?;
    let unit_role = take_locator_text(&mut fields, "unit_role")?;
    RpgMakerProjectionCodec::decode_role(&unit_role).map_err(|source| {
        ProjectLuaCallError::new(
            "invalid_locator",
            format!("RPG Maker locator.unit_role 无效：{source}"),
        )
    })?;
    Ok(ParsedLocator {
        owner,
        group_location,
        unit_role,
    })
}

fn take_locator_text(
    fields: &mut BTreeMap<String, ProjectLuaValue>,
    field: &'static str,
) -> Result<String, ProjectLuaCallError> {
    let Some(ProjectLuaValue::Text(value)) = fields.remove(field) else {
        return Err(ProjectLuaCallError::new(
            "invalid_locator",
            format!("RPG Maker locator.{field} 必须是字符串"),
        ));
    };
    if value.is_empty() || value.chars().all(char::is_whitespace) {
        return Err(ProjectLuaCallError::new(
            "invalid_locator",
            format!("RPG Maker locator.{field} 不能为空白"),
        ));
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

impl LoadedUnit {
    fn identity(&self) -> TranslationUnitIdentity {
        TranslationUnitIdentity::new(
            self.owner,
            self.kind,
            self.group_location.clone(),
            self.role.clone(),
            self.source_content.clone(),
            self.source_context_json.clone(),
        )
    }
}

fn load_unit(
    connection: &Connection,
    locator: &ParsedLocator,
) -> Result<LoadedUnit, ProjectLuaCallError> {
    type UnitRow = (String, String, String, String, String, String, String);
    let row: Option<UnitRow> = connection
        .query_row(
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
              AND text_group.group_location = text_unit.group_location
             CROSS JOIN main.metadata
             JOIN main.rpg_maker_translation_resource AS resource
               ON resource.resource_kind = 'placeholder_rules'
             WHERE text_unit.owner = ?1
               AND text_unit.group_location = ?2
               AND text_unit.unit_role = ?3",
            params![
                locator.owner.storage_name(),
                locator.group_location,
                locator.unit_role
            ],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .optional()
        .map_err(|source| {
            ProjectLuaCallError::new("sqlite", format!("读取 RPG Maker Lua Unit 失败：{source}"))
        })?;
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
        return Err(ProjectLuaCallError::new(
            "unit_not_found",
            "RPG Maker locator 没有命中唯一 Unit",
        ));
    };
    let owner = RpgMakerAssetOwner::from_storage_name(&owner_raw)
        .ok_or_else(|| invalid_database("Unit owner 无效"))?;
    let kind = TextGroupKind::from_storage_name(&kind_raw)
        .ok_or_else(|| invalid_database("Unit group_kind 无效"))?;
    let group_location = RpgMakerLocationCodec::decode(&locator.group_location)
        .map_err(|source| invalid_database(format!("Unit group_location 无效：{source}")))?;
    let role = RpgMakerProjectionCodec::decode_role(&locator.unit_role)
        .map_err(|source| invalid_database(format!("Unit role 无效：{source}")))?;
    let source_content: TextUnitContent = serde_json::from_str(&source_content_json)
        .map_err(|source| invalid_database(format!("Unit source_content_json 无效：{source}")))?;
    validate_text_unit_content_structure(kind, &role, TextUnitContentView::from(&source_content))
        .map_err(|source| invalid_database(format!("Unit 原文结构无效：{source:?}")))?;
    if source_content.is_blank() {
        return Err(invalid_database("Unit 原文不能为空白"));
    }
    let context: serde_json::Value = serde_json::from_str(&source_context_json)
        .map_err(|source| invalid_database(format!("Unit source_context_json 无效：{source}")))?;
    if !context.is_object() {
        return Err(invalid_database(
            "Unit source_context_json 必须是 JSON object",
        ));
    }
    let source = parse_canonical_language(&source_language, "source_language")?;
    let target = parse_canonical_language(&target_language, "target_language")?;
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
) -> Result<TextUnitContent, ProjectLuaCallError> {
    match (source, value) {
        (TextUnitContent::Value(_), ProjectLuaValue::Text(value)) => {
            Ok(TextUnitContent::Value(value))
        }
        (TextUnitContent::Lines(_), ProjectLuaValue::Array(values)) => values
            .into_iter()
            .map(|value| match value {
                ProjectLuaValue::Text(value) => Ok(value),
                _ => Err(ProjectLuaCallError::new(
                    "invalid_translation",
                    "RPG Maker 行数组的每一项都必须是 UTF-8 字符串",
                )),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(TextUnitContent::Lines),
        (TextUnitContent::Value(_), _) => Err(ProjectLuaCallError::new(
            "invalid_translation",
            "这个 RPG Maker Unit 的 translation 必须是 UTF-8 字符串",
        )),
        (TextUnitContent::Lines(_), _) => Err(ProjectLuaCallError::new(
            "invalid_translation",
            "这个 RPG Maker Unit 的 translation 必须是无洞字符串数组",
        )),
    }
}

fn validate_translation_structure(
    kind: TextGroupKind,
    role: &TextUnitRole,
    source: &TextUnitContent,
    translation: &TextUnitContent,
) -> Result<(), ProjectLuaCallError> {
    if translation.is_blank() {
        return Err(ProjectLuaCallError::new(
            "invalid_translation",
            "RPG Maker translation 不能为空白",
        ));
    }
    let contains_forbidden = match translation {
        TextUnitContent::Value(value) => value.contains('\r') || value.contains('\0'),
        TextUnitContent::Lines(lines) => lines.iter().any(|line| {
            line.chars()
                .any(|character| matches!(character, '\r' | '\n' | '\0'))
        }),
    };
    if contains_forbidden {
        return Err(ProjectLuaCallError::new(
            "invalid_translation",
            "RPG Maker translation 包含该结构不允许的 CR、LF 或 NUL",
        ));
    }
    validate_text_unit_content_structure(kind, role, TextUnitContentView::from(translation))
        .map_err(|source| {
            ProjectLuaCallError::new(
                "invalid_translation",
                format!("RPG Maker translation 结构与 Unit 不匹配：{source:?}"),
            )
        })?;
    if matches!(role, TextUnitRole::Choices | TextUnitRole::ScrollingText) {
        let source_lines = source.as_lines().expect("严格对齐角色的原文必须是行数组");
        let translation_lines = translation
            .as_lines()
            .expect("严格对齐角色的译文必须是行数组");
        if source_lines.len() != translation_lines.len() {
            return Err(ProjectLuaCallError::new(
                "invalid_translation",
                format!(
                    "严格对齐 Unit 的译文项数必须是 {}，实际为 {}",
                    source_lines.len(),
                    translation_lines.len()
                ),
            ));
        }
        if source_lines
            .iter()
            .zip(translation_lines)
            .any(|(source, translation)| source.trim().is_empty() != translation.trim().is_empty())
        {
            return Err(ProjectLuaCallError::new(
                "invalid_translation",
                "严格对齐 Unit 的空白槽位置不能改变",
            ));
        }
    }
    Ok(())
}

fn validate_manual_placeholders(
    engine: RpgMakerEngine,
    kind: TextGroupKind,
    source: &TextUnitContent,
    translation: &TextUnitContent,
    canonical_json: &str,
) -> Result<Vec<AppliedPlaceholder>, ProjectLuaCallError> {
    let service = Pcre2PlaceholderService::new().map_err(|source| {
        invalid_database(format!("无法建立 RPG Maker 内置 Placeholder：{source}"))
    })?;
    let custom = compile_placeholder_rules(&service, canonical_json)?;
    validate_manual_placeholders_with_rules(&service, &custom, engine, kind, source, translation)
}

fn validate_manual_placeholders_with_rules(
    service: &Pcre2PlaceholderService,
    custom: &CompiledPlaceholderRules,
    engine: RpgMakerEngine,
    kind: TextGroupKind,
    source: &TextUnitContent,
    translation: &TextUnitContent,
) -> Result<Vec<AppliedPlaceholder>, ProjectLuaCallError> {
    let source_placeholders = protect_content(service, engine, kind, source, custom)?;
    let translation_placeholders = protect_content(service, engine, kind, translation, custom)?;
    if placeholder_multiset(&source_placeholders) != placeholder_multiset(&translation_placeholders)
    {
        return Err(ProjectLuaCallError::new(
            "placeholder_mismatch",
            "人工译文没有完整保留原文实际命中的 Placeholder",
        ));
    }
    Ok(source_placeholders)
}

fn compile_placeholder_rules(
    service: &Pcre2PlaceholderService,
    canonical_json: &str,
) -> Result<CompiledPlaceholderRules, ProjectLuaCallError> {
    let definitions: Vec<PlaceholderRuleDefinition> = serde_json::from_str(canonical_json)
        .map_err(|source| invalid_database(format!("Placeholder 资源 JSON 无效：{source}")))?;
    if serde_json::to_string(&definitions)
        .map_err(|source| invalid_database(format!("无法重新编码 Placeholder 资源：{source}")))?
        != canonical_json
    {
        return Err(invalid_database("Placeholder 资源不是规范紧凑 JSON"));
    }
    service
        .compile_custom(definitions)
        .map_err(|source| invalid_database(format!("Placeholder 资源语义无效：{source}")))
}

fn protect_content(
    service: &Pcre2PlaceholderService,
    engine: RpgMakerEngine,
    kind: TextGroupKind,
    content: &TextUnitContent,
    custom: &CompiledPlaceholderRules,
) -> Result<Vec<AppliedPlaceholder>, ProjectLuaCallError> {
    let (text, line_boundaries) = content_text_and_line_boundaries(content);
    service
        .protect_with_line_boundaries(engine, kind, &text, &line_boundaries, custom)
        .map(|protected| protected.into_parts().1)
        .map_err(|source| {
            ProjectLuaCallError::new(
                "placeholder",
                format!("无法按当前规则保护 RPG Maker 文本：{source}"),
            )
        })
}

fn content_text_and_line_boundaries(content: &TextUnitContent) -> (String, Vec<usize>) {
    match content {
        TextUnitContent::Value(value) => (value.clone(), Vec::new()),
        TextUnitContent::Lines(lines) => {
            let mut offsets = Vec::with_capacity(lines.len().saturating_sub(1));
            let mut cursor = 0;
            for line in lines.iter().take(lines.len().saturating_sub(1)) {
                cursor += line.len();
                offsets.push(cursor);
                cursor += 1;
            }
            (lines.join("\n"), offsets)
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct PlaceholderFact {
    original: String,
    origin: PlaceholderRuleOrigin,
    label: String,
    scope: String,
    segment: PlaceholderSegment,
}

fn placeholder_multiset(bindings: &[AppliedPlaceholder]) -> HashMap<PlaceholderFact, usize> {
    let mut result = HashMap::with_capacity(bindings.len());
    for binding in bindings {
        *result
            .entry(PlaceholderFact {
                original: binding.original().to_owned(),
                origin: binding.origin(),
                label: binding.label().to_owned(),
                scope: binding.scope().to_owned(),
                segment: binding.segment(),
            })
            .or_default() += 1;
    }
    result
}

fn manual_state_error(source: ManualTranslationStateError) -> ProjectLuaCallError {
    ProjectLuaCallError::new("translation_state", source.to_string())
}

fn invalid_database(message: impl Into<String>) -> ProjectLuaCallError {
    ProjectLuaCallError::new("rpg_maker_project", message)
}

fn parse_canonical_language(
    value: &str,
    field: &'static str,
) -> Result<LanguageId, ProjectLuaCallError> {
    let language = LanguageId::parse(value)
        .map_err(|source| invalid_database(format!("{field} 无效：{source}")))?;
    if language.as_str() != value {
        return Err(invalid_database(format!("{field} 不是规范语言 ID")));
    }
    Ok(language)
}

fn capture_rpg_maker_translation_baseline(
    connection: &Connection,
    engine: RpgMakerEngine,
) -> Result<RpgMakerTranslationBaseline, ProjectLuaCallError> {
    let metadata = connection
        .prepare(
            "SELECT source_language, target_language
             FROM main.metadata",
        )
        .and_then(|mut statement| {
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .map_err(|source| invalid_database(format!("读取脚本前语言对失败：{source}")))?;
    let [(source_language, target_language)] = metadata.as_slice() else {
        return Err(invalid_database("脚本前 metadata 必须恰好包含一行"));
    };
    parse_canonical_language(source_language, "metadata.source_language")?;
    parse_canonical_language(target_language, "metadata.target_language")?;

    let placeholder_rows = connection
        .prepare(
            "SELECT canonical_json
             FROM main.rpg_maker_translation_resource
             WHERE resource_kind = 'placeholder_rules'",
        )
        .and_then(|mut statement| {
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .map_err(|source| invalid_database(format!("读取脚本前 Placeholder 资源失败：{source}")))?;
    let [placeholder_json] = placeholder_rows.as_slice() else {
        return Err(invalid_database(
            "脚本前 placeholder_rules 必须恰好包含一项",
        ));
    };
    let placeholder_service = Pcre2PlaceholderService::new().map_err(|source| {
        invalid_database(format!("无法建立 RPG Maker 内置 Placeholder：{source}"))
    })?;
    let placeholder_rules = compile_placeholder_rules(&placeholder_service, placeholder_json)?;

    type CurrentRow = (
        String,
        String,
        String,
        String,
        String,
        String,
        Option<String>,
        Option<Vec<u8>>,
    );
    let current_rows = connection
        .prepare(
            "SELECT unit.owner, unit.group_location, unit.unit_role,
                    text_group.group_kind, unit.source_content_json,
                    unit.source_context_json, unit.translation_content_json,
                    unit.translation_state
             FROM main.rpg_maker_text_unit AS unit
             JOIN main.rpg_maker_text_group AS text_group
               ON text_group.owner = unit.owner
              AND text_group.group_location = unit.group_location
             WHERE unit.translation_content_json IS NOT NULL
                OR unit.translation_state IS NOT NULL",
        )
        .and_then(|mut statement| {
            statement
                .query_map([], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<CurrentRow>>>()
        })
        .map_err(|source| invalid_database(format!("读取脚本前 Current 失败：{source}")))?;

    let mut currents = HashMap::with_capacity(current_rows.len());
    for (
        owner_raw,
        group_location,
        unit_role,
        group_kind,
        source_content_json,
        source_context_json,
        translation_content_json,
        translation_state,
    ) in current_rows
    {
        let (Some(translation_content_json), Some(translation_state)) =
            (translation_content_json, translation_state)
        else {
            return Err(invalid_database(
                "脚本前 Unit 译文与状态必须同时存在或同时为空",
            ));
        };
        Sha256Fingerprint::from_slice(&translation_state)
            .map_err(|source| invalid_database(format!("脚本前 Unit 译文状态无效：{source}")))?;
        RpgMakerAssetOwner::from_storage_name(&owner_raw)
            .ok_or_else(|| invalid_database("脚本前 Unit owner 无效"))?;
        let kind = TextGroupKind::from_storage_name(&group_kind)
            .ok_or_else(|| invalid_database("脚本前 Unit group_kind 无效"))?;
        RpgMakerLocationCodec::decode(&group_location)
            .map_err(|source| invalid_database(format!("脚本前 Unit 位置无效：{source}")))?;
        let role = RpgMakerProjectionCodec::decode_role(&unit_role)
            .map_err(|source| invalid_database(format!("脚本前 Unit role 无效：{source}")))?;
        let source: TextUnitContent = serde_json::from_str(&source_content_json)
            .map_err(|source| invalid_database(format!("脚本前 Unit 原文 JSON 无效：{source}")))?;
        let translation: TextUnitContent = serde_json::from_str(&translation_content_json)
            .map_err(|source| invalid_database(format!("脚本前 Unit 译文 JSON 无效：{source}")))?;
        let context: serde_json::Value =
            serde_json::from_str(&source_context_json).map_err(|source| {
                invalid_database(format!("脚本前 Unit 上下文 JSON 无效：{source}"))
            })?;
        if !context.is_object() {
            return Err(invalid_database("脚本前 Unit 上下文必须是 JSON object"));
        }
        validate_translation_structure(kind, &role, &source, &translation)?;
        let placeholders = validate_manual_placeholders_with_rules(
            &placeholder_service,
            &placeholder_rules,
            engine,
            kind,
            &source,
            &translation,
        )?;
        let key = (owner_raw, group_location, unit_role);
        let baseline = RpgMakerCurrentBaseline {
            group_kind,
            source_content_json,
            source_context_json,
            translation_state,
            placeholders: placeholder_multiset(&placeholders),
        };
        if currents.insert(key, baseline).is_some() {
            return Err(invalid_database("脚本前 Unit 身份重复"));
        }
    }

    Ok(RpgMakerTranslationBaseline {
        source_language: source_language.clone(),
        target_language: target_language.clone(),
        currents,
    })
}

fn validate_rpg_maker_project(
    connection: &Connection,
    expected_project_name: &str,
    engine: RpgMakerEngine,
    translation_baseline: &RpgMakerTranslationBaseline,
) -> Result<(), ProjectLuaCallError> {
    let resources = validate_metadata_and_resources(connection, expected_project_name)?;
    validate_run_plans(connection)?;
    validate_assets(connection, engine, &resources, translation_baseline)
}

struct ValidatedRpgMakerResources {
    dialogue_definition_json: String,
    language_pair: LanguagePair,
    placeholder_service: Pcre2PlaceholderService,
    placeholder_rules: CompiledPlaceholderRules,
}

fn validate_metadata_and_resources(
    connection: &Connection,
    expected_project_name: &str,
) -> Result<ValidatedRpgMakerResources, ProjectLuaCallError> {
    let metadata = connection
        .prepare(
            "SELECT name, source_language, target_language,
                    source_snapshot_fingerprint
             FROM main.metadata",
        )
        .and_then(|mut statement| {
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .map_err(|source| invalid_database(format!("读取 metadata 失败：{source}")))?;
    let [(name, source_language, target_language, source_fingerprint)] = metadata.as_slice() else {
        return Err(invalid_database("metadata 必须恰好包含一行"));
    };
    name.parse::<ProjectName>()
        .map_err(|source| invalid_database(format!("metadata.name 无效：{source}")))?;
    if name != expected_project_name {
        return Err(invalid_database("metadata.name 与本次命令选中的项目不一致"));
    }
    let source_language = parse_canonical_language(source_language, "metadata.source_language")?;
    let target_language = parse_canonical_language(target_language, "metadata.target_language")?;
    Sha256Fingerprint::from_slice(source_fingerprint)
        .map_err(|source| invalid_database(format!("metadata 来源指纹无效：{source}")))?;

    let resources = connection
        .prepare(
            "SELECT resource_kind, canonical_json
             FROM main.rpg_maker_translation_resource
             ORDER BY resource_kind",
        )
        .and_then(|mut statement| {
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .map_err(|source| invalid_database(format!("读取翻译资源失败：{source}")))?;
    if resources.len() != 2 {
        return Err(invalid_database(
            "rpg_maker_translation_resource 必须恰好包含两项",
        ));
    }
    let terminology = resources
        .iter()
        .find_map(|(kind, json)| (kind == "terminology").then_some(json))
        .ok_or_else(|| invalid_database("缺少 terminology 资源"))?;
    let entries: Vec<TerminologyEntry> = serde_json::from_str(terminology)
        .map_err(|source| invalid_database(format!("terminology 资源 JSON 无效：{source}")))?;
    if serde_json::to_string(&entries)
        .map_err(|source| invalid_database(format!("无法重新编码 terminology：{source}")))?
        != *terminology
    {
        return Err(invalid_database("terminology 资源不是规范紧凑 JSON"));
    }
    compile_terminology(entries)
        .map_err(|source| invalid_database(format!("terminology 资源语义无效：{source}")))?;
    let placeholder = resources
        .iter()
        .find_map(|(kind, json)| (kind == "placeholder_rules").then_some(json))
        .ok_or_else(|| invalid_database("缺少 placeholder_rules 资源"))?;
    let service = Pcre2PlaceholderService::new().map_err(|source| {
        invalid_database(format!("无法建立 RPG Maker 内置 Placeholder：{source}"))
    })?;
    let placeholder_rules = compile_placeholder_rules(&service, placeholder)?;

    let definitions = connection
        .prepare(
            "SELECT definition_kind, canonical_json
             FROM main.rpg_maker_project_definition",
        )
        .and_then(|mut statement| {
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .map_err(|source| invalid_database(format!("读取项目定义失败：{source}")))?;
    let [(kind, canonical_json)] = definitions.as_slice() else {
        return Err(invalid_database(
            "rpg_maker_project_definition 必须恰好包含一项",
        ));
    };
    if kind != "mv_dialogue_rules" {
        return Err(invalid_database("项目定义种类无效"));
    }
    MvDialogueDefinition::from_canonical_json(canonical_json)
        .map_err(|source| invalid_database(format!("MV 对话定义无效：{source}")))?;
    Ok(ValidatedRpgMakerResources {
        dialogue_definition_json: canonical_json.clone(),
        language_pair: LanguagePair::new(source_language, target_language),
        placeholder_service: service,
        placeholder_rules,
    })
}

fn validate_run_plans(connection: &Connection) -> Result<(), ProjectLuaCallError> {
    let init_rows = connection
        .prepare(
            "SELECT singleton, source_path_utf16
             FROM main.init_run_plan",
        )
        .and_then(|mut statement| {
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .map_err(|source| invalid_database(format!("读取 Init 运行方案失败：{source}")))?;
    let [(singleton, source_path)] = init_rows.as_slice() else {
        return Err(invalid_database("Init 运行方案必须恰好包含一项"));
    };
    if *singleton != 1 {
        return Err(invalid_database("Init 运行方案 singleton 必须为 1"));
    }
    decode_init_source_path(source_path.clone())
        .map_err(|source| invalid_database(format!("Init 运行方案无效：{source}")))?;

    let extract: Option<(i64, i64)> = connection
        .query_row(
            "SELECT builtin_enabled, rules_enabled FROM main.extract_run_plan",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|source| invalid_database(format!("读取 Extract 运行方案失败：{source}")))?;
    let rules: Option<String> = connection
        .query_row(
            "SELECT canonical_json FROM main.extract_rules_definition",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|source| invalid_database(format!("读取 Rules 定义失败：{source}")))?;
    match (extract, rules) {
        (None, None) => {}
        (Some((1, 0)), None) => {}
        (Some((builtin, 1)), Some(rules)) if builtin == 0 || builtin == 1 => {
            ExtractRulesCanonicalJson::new(rules)
                .map_err(|source| invalid_database(format!("Rules 运行方案无效：{source}")))?;
        }
        _ => {
            return Err(invalid_database("Extract 运行方案与 Rules 定义不一致"));
        }
    }
    Ok(())
}

#[derive(Clone)]
struct StoredUnit {
    role_raw: String,
    source_json: String,
    context_json: String,
    unit_order: usize,
    write_back: RpgMakerWriteBackUnit,
}

struct StoredGroup {
    owner: RpgMakerAssetOwner,
    location_raw: String,
    location: crate::rpg_maker::text::RpgMakerLocation,
    group_order: usize,
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
) -> Result<(), ProjectLuaCallError> {
    let owner_rows = connection
        .prepare(
            "SELECT owner, source_snapshot_fingerprint, asset_snapshot_fingerprint
             FROM main.rpg_maker_asset_owner_state
             ORDER BY CASE owner WHEN 'builtin' THEN 0 WHEN 'rules' THEN 1 END",
        )
        .and_then(|mut statement| {
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .map_err(|source| invalid_database(format!("读取资产 owner 失败：{source}")))?;
    let mut owner_fingerprints = HashMap::new();
    for (owner_raw, source_fingerprint, asset_fingerprint) in owner_rows {
        let owner = RpgMakerAssetOwner::from_storage_name(&owner_raw)
            .ok_or_else(|| invalid_database("资产 owner 无效"))?;
        Sha256Fingerprint::from_slice(&source_fingerprint)
            .map_err(|source| invalid_database(format!("owner 来源指纹无效：{source}")))?;
        let asset = Sha256Fingerprint::from_slice(&asset_fingerprint)
            .map_err(|source| invalid_database(format!("owner 资产指纹无效：{source}")))?;
        if owner_fingerprints.insert(owner, asset).is_some() {
            return Err(invalid_database("资产 owner 重复"));
        }
    }

    let group_rows = connection
        .prepare(
            "SELECT owner, group_location, group_order, group_kind,
                    projection_recipe_json
             FROM main.rpg_maker_text_group
             ORDER BY CASE owner WHEN 'builtin' THEN 0 WHEN 'rules' THEN 1 END,
                      group_order",
        )
        .and_then(|mut statement| {
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .map_err(|source| invalid_database(format!("读取 RPG Maker Group 失败：{source}")))?;
    let mut next_group_orders = HashMap::<RpgMakerAssetOwner, usize>::new();
    let mut groups = Vec::with_capacity(group_rows.len());
    let mut group_indexes = HashMap::with_capacity(group_rows.len());
    for (owner_raw, location_raw, group_order, kind_raw, recipes_raw) in group_rows {
        let owner = RpgMakerAssetOwner::from_storage_name(&owner_raw)
            .ok_or_else(|| invalid_database("Group owner 无效"))?;
        if !owner_fingerprints.contains_key(&owner) {
            return Err(invalid_database("Group 引用了未激活的 owner"));
        }
        let group_order = usize::try_from(group_order)
            .map_err(|_| invalid_database("Group order 不是非负序号"))?;
        let expected = next_group_orders.entry(owner).or_default();
        if group_order != *expected {
            return Err(invalid_database(format!(
                "{} Group order 必须从 0 连续",
                owner.storage_name()
            )));
        }
        *expected += 1;
        let location = RpgMakerLocationCodec::decode(&location_raw)
            .map_err(|source| invalid_database(format!("Group 位置无效：{source}")))?;
        let kind = TextGroupKind::from_storage_name(&kind_raw)
            .ok_or_else(|| invalid_database("Group kind 无效"))?;
        let recipes = RpgMakerProjectionCodec::decode_recipes(&recipes_raw)
            .map_err(|source| invalid_database(format!("Group 配方无效：{source}")))?;
        let index = groups.len();
        if group_indexes
            .insert((owner, location_raw.clone()), index)
            .is_some()
        {
            return Err(invalid_database("Group 身份重复"));
        }
        groups.push(StoredGroup {
            owner,
            location_raw,
            location,
            group_order,
            kind_raw,
            kind,
            recipes_raw,
            recipes,
            units: Vec::new(),
        });
    }

    let unit_rows = connection
        .prepare(
            "SELECT unit.owner, unit.group_location, unit.unit_role, unit.unit_order,
                    unit.source_content_json, unit.source_context_json,
                    unit.translation_content_json, unit.translation_state
             FROM main.rpg_maker_text_unit AS unit
             JOIN main.rpg_maker_text_group AS text_group
               ON text_group.owner = unit.owner
              AND text_group.group_location = unit.group_location
             ORDER BY CASE unit.owner WHEN 'builtin' THEN 0 WHEN 'rules' THEN 1 END,
                      text_group.group_order, unit.unit_order",
        )
        .and_then(|mut statement| {
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, Option<Vec<u8>>>(7)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .map_err(|source| invalid_database(format!("读取 RPG Maker Unit 失败：{source}")))?;
    for (
        owner_raw,
        location_raw,
        role_raw,
        unit_order,
        source_json,
        context_json,
        translation_json,
        translation_state,
    ) in unit_rows
    {
        let owner = RpgMakerAssetOwner::from_storage_name(&owner_raw)
            .ok_or_else(|| invalid_database("Unit owner 无效"))?;
        let index = group_indexes
            .get(&(owner, location_raw.clone()))
            .copied()
            .ok_or_else(|| invalid_database("Unit 缺少所属 Group"))?;
        let group = &mut groups[index];
        let unit_order =
            usize::try_from(unit_order).map_err(|_| invalid_database("Unit order 不是非负序号"))?;
        if unit_order != group.units.len() {
            return Err(invalid_database("Unit order 必须从 0 连续"));
        }
        let role = RpgMakerProjectionCodec::decode_role(&role_raw)
            .map_err(|source| invalid_database(format!("Unit role 无效：{source}")))?;
        let source: TextUnitContent = serde_json::from_str(&source_json)
            .map_err(|source| invalid_database(format!("Unit 原文 JSON 无效：{source}")))?;
        let translation: Option<TextUnitContent> = translation_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|source| invalid_database(format!("Unit 译文 JSON 无效：{source}")))?;
        match (translation.as_ref(), translation_state.as_ref()) {
            (None, None) => {}
            (Some(_), Some(state)) => {
                Sha256Fingerprint::from_slice(state)
                    .map_err(|source| invalid_database(format!("Unit 译文状态无效：{source}")))?;
            }
            _ => return Err(invalid_database("Unit 译文与状态必须同时存在或同时为空")),
        }
        let context: serde_json::Value = serde_json::from_str(&context_json)
            .map_err(|source| invalid_database(format!("Unit 上下文 JSON 无效：{source}")))?;
        if !context.is_object() {
            return Err(invalid_database("Unit 上下文必须是 JSON object"));
        }
        if let (Some(translation), Some(state)) = (translation.as_ref(), translation_state.as_ref())
        {
            validate_translation_structure(group.kind, &role, &source, translation)?;
            let placeholders = validate_manual_placeholders_with_rules(
                &resources.placeholder_service,
                &resources.placeholder_rules,
                engine,
                group.kind,
                &source,
                translation,
            )?;
            let placeholder_facts = placeholder_multiset(&placeholders);
            let key = (owner_raw.clone(), location_raw.clone(), role_raw.clone());
            let unchanged_current = translation_baseline
                .currents
                .get(&key)
                .filter(|baseline| baseline.translation_state == *state);
            if let Some(baseline) = unchanged_current {
                let unchanged_semantics = baseline.group_kind == group.kind_raw
                    && baseline.source_content_json == source_json
                    && baseline.source_context_json == context_json
                    && resources.language_pair.source().as_str()
                        == translation_baseline.source_language
                    && resources.language_pair.target().as_str()
                        == translation_baseline.target_language
                    && baseline.placeholders == placeholder_facts;
                if !unchanged_semantics {
                    return Err(invalid_database(
                        "已有 Current 只允许在语义事实不变时保留原 translation_state",
                    ));
                }
            } else {
                let identity = TranslationUnitIdentity::new(
                    owner,
                    group.kind,
                    group.location.clone(),
                    role.clone(),
                    source.clone(),
                    context_json.clone(),
                );
                let manual_state = manual_translation_state_fingerprint(
                    engine,
                    &resources.language_pair,
                    &identity,
                    &placeholders,
                )
                .map_err(manual_state_error)?;
                if manual_state.as_bytes().as_slice() != state.as_slice() {
                    return Err(invalid_database(
                        "新增或改写的 translation_state 必须等于当前 Unit 的人工译文状态",
                    ));
                }
            }
        }
        let write_back = RpgMakerWriteBackUnit::new(role, source, translation)
            .map_err(|source| invalid_database(format!("Unit 结构无效：{source}")))?;
        group.units.push(StoredUnit {
            role_raw,
            source_json,
            context_json,
            unit_order,
            write_back,
        });
    }

    let mut fingerprint_builders = HashMap::new();
    for owner in owner_fingerprints.keys().copied() {
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
        fingerprint_builders
            .get_mut(&group.owner)
            .expect("Group owner 已验证")
            .group(
                &group.location_raw,
                group.group_order,
                &group.kind_raw,
                &group.recipes_raw,
            );
    }
    for group in &groups {
        for unit in &group.units {
            fingerprint_builders
                .get_mut(&group.owner)
                .expect("Unit owner 已验证")
                .unit(
                    &group.location_raw,
                    &unit.role_raw,
                    unit.unit_order,
                    &unit.source_json,
                    &unit.context_json,
                );
        }
    }

    let mut logical_claims = HashMap::<RpgMakerAssetOwner, Vec<EncodedMutationClaim>>::new();
    for group in &groups {
        let validated = RpgMakerWriteBackGroup::from_recipes(
            group.kind,
            group.location.clone(),
            group
                .units
                .iter()
                .map(|unit| unit.write_back.clone())
                .collect(),
            group.recipes.clone(),
        )
        .map_err(|source| invalid_database(format!("Group 领域结构无效：{source}")))?;
        for lock in validated.mutation_claims().locks() {
            logical_claims
                .entry(group.owner)
                .or_default()
                .push(EncodedMutationClaim::new(
                    RpgMakerProjectionCodec::encode_mutation_resource(lock.resource()).map_err(
                        |source| invalid_database(format!("无法编码 Group Claim：{source}")),
                    )?,
                    lock.access(),
                    group.location_raw.clone(),
                    group.group_order,
                ));
        }
    }
    for claims in logical_claims.values_mut() {
        sort_logical_claims(claims);
    }

    let claim_rows = connection
        .prepare(
            "SELECT owner, group_location, resource_key, access
             FROM main.rpg_maker_mutation_claim
             ORDER BY CASE owner WHEN 'builtin' THEN 0 WHEN 'rules' THEN 1 END,
                      resource_key, access, group_location",
        )
        .and_then(|mut statement| {
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .map_err(|source| invalid_database(format!("读取 Mutation Claim 失败：{source}")))?;
    let mut stored_claims = HashMap::<RpgMakerAssetOwner, Vec<EncodedMutationClaim>>::new();
    for (owner_raw, location_raw, resource_raw, access_raw) in claim_rows {
        let owner = RpgMakerAssetOwner::from_storage_name(&owner_raw)
            .ok_or_else(|| invalid_database("Claim owner 无效"))?;
        let group_order = group_indexes
            .get(&(owner, location_raw.clone()))
            .and_then(|index| groups.get(*index))
            .map(|group| group.group_order)
            .ok_or_else(|| invalid_database("Claim 缺少所属 Group"))?;
        let resource = RpgMakerProjectionCodec::decode_mutation_resource(&resource_raw)
            .map_err(|source| invalid_database(format!("Claim resource 无效：{source}")))?;
        if RpgMakerProjectionCodec::encode_mutation_resource(&resource)
            .map_err(|source| invalid_database(format!("无法重新编码 Claim resource：{source}")))?
            != resource_raw
        {
            return Err(invalid_database("Claim resource 不是规范编码"));
        }
        let access = MutationResourceAccess::from_storage_name(&access_raw)
            .ok_or_else(|| invalid_database("Claim access 无效"))?;
        stored_claims
            .entry(owner)
            .or_default()
            .push(EncodedMutationClaim::new(
                resource_raw,
                access,
                location_raw,
                group_order,
            ));
    }

    for owner in owner_fingerprints.keys() {
        let logical = logical_claims.remove(owner).unwrap_or_default();
        let expected_summary = collision_summary(&logical)
            .map_err(|source| invalid_database(format!("Claim 语义冲突：{source}")))?;
        let actual_summary = stored_claims.remove(owner).unwrap_or_default();
        if actual_summary != expected_summary {
            return Err(invalid_database("Mutation Claim 摘要与 Group 配方不一致"));
        }
        let builder = fingerprint_builders
            .get_mut(owner)
            .expect("每个 active owner 已建立指纹");
        for claim in &logical {
            builder.claim(
                &claim.resource_key,
                claim.access.storage_name(),
                &claim.group_location,
            );
        }
    }
    if !stored_claims.is_empty() || !logical_claims.is_empty() {
        return Err(invalid_database("Mutation Claim 引用了未激活 owner"));
    }
    for (owner, expected) in owner_fingerprints {
        let actual = fingerprint_builders
            .remove(&owner)
            .expect("每个 active owner 已建立指纹")
            .finish();
        if actual != expected {
            return Err(invalid_database(format!(
                "{} 资产指纹与当前 Group、Unit、Claim 不一致",
                owner.storage_name()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

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
                    0,
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
            0,
            TextGroupKind::DatabaseEntry.storage_name(),
            &recipes_json,
        );
        fingerprint.unit(&group_location, &unit_role, 0, &source_json, context_json);
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
                 (owner, group_location, group_order, group_kind, projection_recipe_json)
                 VALUES ('builtin', ?1, 0, 'database_entry', ?2)",
                params![group_location, recipes_json],
            )
            .expect("应插入 Group");
        transaction
            .execute(
                "INSERT INTO rpg_maker_text_unit
                 (owner, group_location, unit_role, unit_order,
                  source_content_json, source_context_json,
                  translation_content_json, translation_state)
                 VALUES ('builtin', ?1, ?2, 0, ?3, ?4, NULL, NULL)",
                params![group_location, unit_role, source_json, context_json],
            )
            .expect("应插入 Unit");
        for claim in summary {
            transaction
                .execute(
                    "INSERT INTO rpg_maker_mutation_claim
                     (owner, group_location, resource_key, access)
                     VALUES ('builtin', ?1, ?2, ?3)",
                    params![
                        claim.group_location,
                        claim.resource_key,
                        claim.access.storage_name()
                    ],
                )
                .expect("应插入 Claim");
        }
        transaction.commit().expect("应提交测试资产");
        (group_location, unit_role)
    }

    fn locator(project: &TestProject) -> String {
        format!(
            "{{owner = \"builtin\", group_location = [=[{}]=], unit_role = [=[{}]=]}}",
            project.group_location, project.unit_role
        )
    }

    fn run(
        project: &TestProject,
        source: &str,
    ) -> Result<super::super::ProjectLuaRunReport, ProjectLuaRunError> {
        let connection = Connection::open(&project.database_path).expect("应打开项目数据库");
        run_project_lua(
            connection,
            ProjectLuaRunRequest::new(
                ProjectLuaProject::new("game", "mz"),
                ProjectLuaProgram::new("rpg.lua", source.as_bytes(), Vec::new()),
                rpg_maker_project_lua_adapter(RpgMakerEngine::Mz),
            ),
        )
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
                "SELECT translation_content_json, translation_state
                 FROM rpg_maker_text_unit
                 WHERE owner = 'builtin' AND group_location = ?1 AND unit_role = ?2",
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
                        WHERE owner = 'builtin' AND group_location = ?2 AND unit_role = ?3]=],
                     {{[=["人工修订 \\V[1] {{hero}}"]=], [=[{}]=], [=[{}]=]}}
                   )"#,
                project.group_location, project.unit_role
            ),
        )
        .expect("直接 SQL 修订已有译文应保留 Current state");
        let connection = Connection::open(&project.database_path).expect("应重开数据库");
        let (translation_after, state_after): (String, Vec<u8>) = connection
            .query_row(
                "SELECT translation_content_json, translation_state
                 FROM rpg_maker_text_unit
                 WHERE owner = 'builtin' AND group_location = ?1 AND unit_role = ?2",
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
                        WHERE owner = 'builtin' AND group_location = ?2 AND unit_role = ?3]=],
                     {{ctx.db.blob(string.rep("x", 32)), [=[{}]=], [=[{}]=]}}
                   )"#,
                project.group_location, project.unit_role
            ),
        )
        .expect_err("直接 SQL 不能伪造新的 Current 状态");
        assert!(matches!(
            state_error,
            ProjectLuaRunError::RolledBack(ProjectLuaFailure::Host {
                operation: "translation.validate",
                ..
            })
        ));

        run(
            &project,
            &format!("ctx.translation.clear({})", locator(&project)),
        )
        .expect("typed clear 应成功");
        let cleared: Option<String> = Connection::open(&project.database_path)
            .expect("应重开数据库")
            .query_row(
                "SELECT translation_content_json FROM rpg_maker_text_unit
                 WHERE owner = 'builtin' AND group_location = ?1 AND unit_role = ?2",
                params![project.group_location, project.unit_role],
                |row| row.get(0),
            )
            .expect("应读取清理结果");
        assert_eq!(cleared, None);
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
                Err(ProjectLuaRunError::RolledBack(
                    ProjectLuaFailure::Host { .. }
                ))
            ));
        }
        let forged_state = format!(
            r#"ctx.db.execute(
                 [=[UPDATE main.rpg_maker_text_unit
                    SET translation_content_json = ?1, translation_state = ?2
                    WHERE owner = 'builtin' AND group_location = ?3 AND unit_role = ?4]=],
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
            Err(ProjectLuaRunError::RolledBack(ProjectLuaFailure::Host {
                operation: "translation.validate",
                ..
            }))
        ));
        let translation: Option<String> = Connection::open(&project.database_path)
            .expect("应重开数据库")
            .query_row(
                "SELECT translation_content_json FROM rpg_maker_text_unit
                 WHERE owner = 'builtin' AND group_location = ?1 AND unit_role = ?2",
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
            ProjectLuaRunError::RolledBack(ProjectLuaFailure::Host {
                operation: "translation.validate",
                ..
            })
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
    fn final_validation_preserves_selected_project_identity_and_init_run_plan() {
        let project = create_project();

        let name_error = run(
            &project,
            r#"ctx.db.execute("UPDATE main.metadata SET name = 'other-project'")"#,
        )
        .expect_err("不能把项目数据库改成另一个项目");
        assert!(matches!(
            name_error,
            ProjectLuaRunError::RolledBack(ProjectLuaFailure::Host {
                operation: "translation.validate",
                ..
            })
        ));

        let missing_init_error = run(
            &project,
            r#"ctx.db.execute("DELETE FROM main.init_run_plan")"#,
        )
        .expect_err("不能删除 Init 运行方案");
        assert!(matches!(
            missing_init_error,
            ProjectLuaRunError::RolledBack(ProjectLuaFailure::Host {
                operation: "translation.validate",
                ..
            })
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
            ProjectLuaRunError::RolledBack(ProjectLuaFailure::Host {
                operation: "translation.validate",
                ..
            })
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
        let source = TextUnitContent::Lines(vec!["选择".to_owned(), String::new()]);
        assert_eq!(
            parse_translation(
                ProjectLuaValue::Array(vec![
                    ProjectLuaValue::Text("选项".to_owned()),
                    ProjectLuaValue::Text(String::new()),
                ]),
                &source,
            )
            .expect("稠密字符串数组应可解析"),
            TextUnitContent::Lines(vec!["选项".to_owned(), String::new()])
        );
        assert!(parse_translation(ProjectLuaValue::Text("选项".to_owned()), &source).is_err());
        let role = TextUnitRole::Choices;
        assert!(
            validate_translation_structure(
                TextGroupKind::EventChoices,
                &role,
                &source,
                &TextUnitContent::Lines(vec!["选项".to_owned(), "新增".to_owned()]),
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
