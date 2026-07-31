//! Generic 项目的原子 Lua 适配器。

use std::sync::{Arc, Mutex};

use rusqlite::types::{Type, ValueRef};
use rusqlite::{Connection, Row, params};

use crate::execution::CooperativeCancellation;
use crate::fingerprint::Sha256Fingerprint;
use crate::generic::{
    GenericCompiledPlaceholderResource, GenericCompiledPlaceholderRules,
    GenericCompiledTerminologyResource, GenericPlaceholderError, GenericPlaceholderService,
    GenericProject, GenericProjectError, TranslationOrigin,
    compiled_placeholder_resource_for_connection_with_cancellation,
    compiled_terminology_resource_for_connection_with_cancellation,
    terminology_hit_fingerprint_with_cancellation,
    validate_current_generic_schema_with_cancellation,
    validate_project_connection_with_compiled_resources_and_cancellation,
    validate_translation_placeholders_and_binding_with_cancellation,
    validated_manual_translation_state_with_compiled_rules_for_connection_with_cancellation,
};
use crate::language::{LanguageText, LanguageTextSegment};
use crate::translation::placeholder::PlaceholderRuleCompilationError;
use crate::translation::planning_resource::CompiledTerminology;

use super::{
    ProjectLuaCallError, ProjectLuaDatabasePrerequisiteError, ProjectLuaEngineAdapter,
    ProjectLuaSchemaObjectKind, ProjectLuaValue, project_lua_object_contains_field,
    project_lua_worker_spawn_message, take_project_lua_object_field,
};

const GENERIC_ATT_TABLES: &[&str] = &[
    "generic_project",
    "generic_file",
    "generic_group",
    "generic_unit",
    "translation_resource",
];
const CANCELLATION_TEXT_CHUNK_BYTES: usize = 64 * 1024;

type InitialTranslationStates = Vec<InitialTranslationEntry>;

struct InitialTranslationEntry {
    group_id: String,
    unit_id: String,
    state: Option<InitialTranslationState>,
}

/// Generic 项目数据库的 typed translation 与最终校验。
pub(crate) struct GenericProjectLuaAdapter {
    expected_project: GenericProject,
    cancellation: CooperativeCancellation,
    baseline: Mutex<Option<GenericProjectLuaBaseline>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InitialTranslationState {
    origin: TranslationOrigin,
    state: Sha256Fingerprint,
    placeholder_binding: Sha256Fingerprint,
    automatic_terminology_hits: Option<Sha256Fingerprint>,
}

struct GenericProjectLuaBaseline {
    translations: InitialTranslationStates,
    placeholder: GenericCompiledPlaceholderResource,
    terminology: GenericCompiledTerminologyResource,
}

impl GenericProjectLuaAdapter {
    pub(crate) fn new(
        expected_project: GenericProject,
        cancellation: CooperativeCancellation,
    ) -> Self {
        Self {
            expected_project,
            cancellation,
            baseline: Mutex::new(None),
        }
    }

    fn placeholder_resource_for_connection(
        &self,
        connection: &Connection,
    ) -> Result<GenericCompiledPlaceholderResource, ProjectLuaCallError> {
        ensure_generic_lua_running(&self.cancellation)?;
        let cached = {
            let baseline = self
                .baseline
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let cache = &baseline.as_ref().ok_or_else(|| {
                ProjectLuaCallError::new("generic_project", "缺少 Generic Lua 脚本前译文状态")
            })?;
            cache.placeholder.clone()
        };
        let current = compiled_placeholder_resource_for_connection_with_cancellation(
            connection,
            Some(&cached),
            &self.cancellation,
        )
        .map_err(generic_error)?;
        ensure_generic_lua_running(&self.cancellation)?;
        let mut baseline = self
            .baseline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        baseline
            .as_mut()
            .ok_or_else(|| {
                ProjectLuaCallError::new("generic_project", "缺少 Generic Lua 脚本前译文状态")
            })?
            .placeholder = current.clone();
        ensure_generic_lua_running(&self.cancellation)?;
        Ok(current)
    }

    fn terminology_resource_for_connection(
        &self,
        connection: &Connection,
    ) -> Result<GenericCompiledTerminologyResource, ProjectLuaCallError> {
        ensure_generic_lua_running(&self.cancellation)?;
        let cached = {
            let baseline = self
                .baseline
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            baseline
                .as_ref()
                .ok_or_else(|| {
                    ProjectLuaCallError::new("generic_project", "缺少 Generic Lua 脚本前译文状态")
                })?
                .terminology
                .clone()
        };
        let current = compiled_terminology_resource_for_connection_with_cancellation(
            connection,
            Some(&cached),
            &self.cancellation,
        )
        .map_err(generic_error)?;
        ensure_generic_lua_running(&self.cancellation)?;
        let mut baseline = self
            .baseline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        baseline
            .as_mut()
            .ok_or_else(|| {
                ProjectLuaCallError::new("generic_project", "缺少 Generic Lua 脚本前译文状态")
            })?
            .terminology = current.clone();
        ensure_generic_lua_running(&self.cancellation)?;
        Ok(current)
    }
}

/// 为命令接线建立 Generic Lua 引擎适配器。
pub(crate) fn generic_project_lua_adapter(
    expected_project: GenericProject,
    cancellation: CooperativeCancellation,
) -> Arc<dyn ProjectLuaEngineAdapter> {
    Arc::new(GenericProjectLuaAdapter::new(
        expected_project,
        cancellation,
    ))
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
        ensure_generic_lua_running(&self.cancellation)?;
        let (group_id, unit_id) = parse_locator(locator)?;
        let translation = parse_translation(translation, &self.cancellation)?;
        let placeholder = self.placeholder_resource_for_connection(connection)?;
        let service = placeholder.service();
        let state =
            validated_manual_translation_state_with_compiled_rules_for_connection_with_cancellation(
            connection,
            &group_id,
            &unit_id,
            &translation,
            &service,
            placeholder.compiled(),
            &self.cancellation,
        )
        .map_err(generic_error)?;
        ensure_generic_lua_running(&self.cancellation)?;
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
        ensure_generic_lua_running(&self.cancellation)?;
        Ok(u64::try_from(changed).expect("受支持平台的 usize 必须能表示为 u64"))
    }

    fn clear_translation(
        &self,
        connection: &Connection,
        locator: ProjectLuaValue,
    ) -> Result<u64, ProjectLuaCallError> {
        ensure_generic_lua_running(&self.cancellation)?;
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
        ensure_generic_lua_running(&self.cancellation)?;
        Ok(u64::try_from(changed).expect("受支持平台的 usize 必须能表示为 u64"))
    }

    fn validate_database_before_script(
        &self,
        connection: &Connection,
        project: &super::ProjectLuaProject,
    ) -> Result<(), ProjectLuaDatabasePrerequisiteError> {
        if project.engine() != "generic"
            || project.name() != self.expected_project.project_name().as_str()
        {
            return Err(ProjectLuaDatabasePrerequisiteError::invalid_project_state(
                "Lua 项目身份与打开的 Generic 项目不一致",
            ));
        }
        validate_current_generic_schema_with_cancellation(connection, &self.cancellation)
            .map_err(generic_prerequisite_error)
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
        let placeholder = compiled_placeholder_resource_for_connection_with_cancellation(
            connection,
            None,
            &self.cancellation,
        )
        .map_err(generic_error)?;
        let terminology = compiled_terminology_resource_for_connection_with_cancellation(
            connection,
            None,
            &self.cancellation,
        )
        .map_err(generic_error)?;
        let service = placeholder.service();
        let group_terminology_hits = group_terminology_hits_for_units(
            connection,
            &service,
            placeholder.compiled(),
            terminology.compiled(),
            &self.cancellation,
        )?;
        let translations = capture_initial_translation_states(
            connection,
            &service,
            placeholder.compiled(),
            &group_terminology_hits,
            &self.cancellation,
        )?;
        let baseline = GenericProjectLuaBaseline {
            translations,
            placeholder,
            terminology,
        };
        let mut slot = self
            .baseline
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
        let placeholder = self.placeholder_resource_for_connection(connection)?;
        let terminology = self.terminology_resource_for_connection(connection)?;
        validate_project_connection_with_compiled_resources_and_cancellation(
            connection,
            &self.expected_project,
            &terminology,
            &placeholder,
            &self.cancellation,
        )
        .map_err(generic_error)?;
        let service = placeholder.service();
        let group_terminology_hits = group_terminology_hits_for_units(
            connection,
            &service,
            placeholder.compiled(),
            terminology.compiled(),
            &self.cancellation,
        )?;
        let baseline = self
            .baseline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let baseline = baseline.as_ref().ok_or_else(|| {
            ProjectLuaCallError::new("generic_project", "缺少 Generic Lua 脚本前译文状态")
        })?;
        self.validate_translation_states(
            connection,
            &baseline.translations,
            &service,
            placeholder.compiled(),
            &group_terminology_hits,
        )
    }
}

impl GenericProjectLuaAdapter {
    fn validate_translation_states(
        &self,
        connection: &Connection,
        initial_translations: &InitialTranslationStates,
        service: &GenericPlaceholderService,
        compiled: &GenericCompiledPlaceholderRules,
        group_terminology_hits: &[Sha256Fingerprint],
    ) -> Result<(), ProjectLuaCallError> {
        ensure_generic_lua_running(&self.cancellation)?;
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
        let mut rows = statement.query([]).map_err(|source| {
            ProjectLuaCallError::new("sqlite", format!("检查 Generic 译文失败：{source}"))
        })?;
        let mut initial_index = 0_usize;
        while let Some(row) = rows.next().map_err(|source| {
            ProjectLuaCallError::new("sqlite", format!("读取 Generic 译文失败：{source}"))
        })? {
            ensure_generic_lua_running(&self.cancellation)?;
            let group_id = sqlite_text_with_cancellation(
                row,
                0,
                "group_id",
                "读取 Generic 译文失败",
                &self.cancellation,
            )?;
            let unit_id = sqlite_text_with_cancellation(
                row,
                1,
                "unit_id",
                "读取 Generic 译文失败",
                &self.cancellation,
            )?;
            let kind = sqlite_text_with_cancellation(
                row,
                2,
                "kind",
                "读取 Generic 译文失败",
                &self.cancellation,
            )?;
            let source_text = sqlite_text_with_cancellation(
                row,
                3,
                "source_text",
                "读取 Generic 译文失败",
                &self.cancellation,
            )?;
            let translation = sqlite_optional_text_with_cancellation(
                row,
                4,
                "translation",
                "读取 Generic 译文失败",
                &self.cancellation,
            )?;
            let origin = sqlite_optional_text_with_cancellation(
                row,
                5,
                "translation_origin",
                "读取 Generic 译文失败",
                &self.cancellation,
            )?;
            let state = sqlite_optional_blob_with_cancellation(
                row,
                6,
                "translation_state",
                "读取 Generic 译文失败",
                &self.cancellation,
            )?;
            ensure_generic_lua_running(&self.cancellation)?;
            let initial_entry = initial_translations.get(initial_index).ok_or_else(|| {
                ProjectLuaCallError::new(
                    "translation_state",
                    "Lua 修改了 Generic Unit 身份或新增了未受管 Unit",
                )
            })?;
            let group_terminology_hit = group_terminology_hits
                .get(initial_index)
                .copied()
                .ok_or_else(|| {
                    ProjectLuaCallError::new(
                        "translation_state",
                        "Generic Group 术语命中状态缺少对应 Unit",
                    )
                })?;
            initial_index += 1;
            if !generic_text_eq_with_cancellation(
                &initial_entry.group_id,
                &group_id,
                &self.cancellation,
            )? || !generic_text_eq_with_cancellation(
                &initial_entry.unit_id,
                &unit_id,
                &self.cancellation,
            )? {
                return Err(ProjectLuaCallError::new(
                    "translation_state",
                    "Lua 修改了 Generic Unit 身份、顺序或新增了未受管 Unit",
                ));
            }
            let initial = &initial_entry.state;
            let Some(translation) = translation else {
                if origin.is_some() || state.is_some() {
                    return Err(ProjectLuaCallError::new(
                        "translation_state",
                        "Generic 空译文仍带有 origin 或 state",
                    ));
                }
                continue;
            };
            validate_translation_text(&translation, &self.cancellation)?;
            ensure_generic_lua_running(&self.cancellation)?;
            let placeholder_binding =
                validate_translation_placeholders_and_binding_with_cancellation(
                    service,
                    compiled,
                    &kind,
                    &source_text,
                    &translation,
                    || ensure_generic_lua_running(&self.cancellation),
                )?
                .map_err(generic_placeholder_error)?;
            ensure_generic_lua_running(&self.cancellation)?;
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
            let state_unchanged = initial.as_ref().is_some_and(|initial| {
                initial.origin == origin
                    && initial.state == state
                    && initial.placeholder_binding == placeholder_binding
                    && (origin == TranslationOrigin::Manual
                        || initial.automatic_terminology_hits == Some(group_terminology_hit))
            });
            if state_unchanged {
                continue;
            }
            if origin != TranslationOrigin::Manual {
                return Err(ProjectLuaCallError::new(
                    "translation_state",
                    "新增或改变状态的 Generic 译文必须通过人工状态校验",
                ));
            }
            ensure_generic_lua_running(&self.cancellation)?;
            let expected =
                validated_manual_translation_state_with_compiled_rules_for_connection_with_cancellation(
                    connection,
                    &group_id,
                    &unit_id,
                    &translation,
                    service,
                    compiled,
                    &self.cancellation,
                )
                .map_err(generic_error)?;
            ensure_generic_lua_running(&self.cancellation)?;
            if state != expected {
                return Err(ProjectLuaCallError::new(
                    "translation_state",
                    "Generic 人工译文状态与当前项目事实不一致",
                ));
            }
        }
        if initial_index != initial_translations.len() {
            return Err(ProjectLuaCallError::new(
                "translation_state",
                "Lua 删除了受管 Generic Unit",
            ));
        }
        if initial_index != group_terminology_hits.len() {
            return Err(ProjectLuaCallError::new(
                "translation_state",
                "Generic Group 术语命中状态与 Unit 数量不一致",
            ));
        }
        ensure_generic_lua_running(&self.cancellation)
    }
}

fn group_terminology_hits_for_units(
    connection: &Connection,
    service: &GenericPlaceholderService,
    placeholder_rules: &GenericCompiledPlaceholderRules,
    terminology: &CompiledTerminology,
    cancellation: &CooperativeCancellation,
) -> Result<Vec<Sha256Fingerprint>, ProjectLuaCallError> {
    ensure_generic_lua_running(cancellation)?;
    let mut statement = connection
        .prepare(
            "SELECT generic_unit.group_id, generic_group.kind, generic_unit.source_text
             FROM main.generic_unit AS generic_unit
             JOIN main.generic_group AS generic_group USING (group_id)
             ORDER BY generic_group.relative_path, generic_group.ordinal,
                      generic_unit.ordinal",
        )
        .map_err(|source| {
            ProjectLuaCallError::new(
                "sqlite",
                format!("准备计算 Generic Group 术语命中失败：{source}"),
            )
        })?;
    let mut rows = statement.query([]).map_err(|source| {
        ProjectLuaCallError::new(
            "sqlite",
            format!("计算 Generic Group 术语命中失败：{source}"),
        )
    })?;
    let mut output = Vec::new();
    let mut current_group_id = None::<String>;
    let mut current_group_texts = Vec::<LanguageText>::new();
    let mut current_group_unit_count = 0_usize;

    while let Some(row) = rows.next().map_err(|source| {
        ProjectLuaCallError::new(
            "sqlite",
            format!("读取 Generic Group 术语命中事实失败：{source}"),
        )
    })? {
        ensure_generic_lua_running(cancellation)?;
        let group_id = sqlite_text_with_cancellation(
            row,
            0,
            "group_id",
            "读取 Generic Group 术语命中事实失败",
            cancellation,
        )?;
        let starts_new_group = match current_group_id.as_deref() {
            Some(current) => !generic_text_eq_with_cancellation(current, &group_id, cancellation)?,
            None => false,
        };
        if starts_new_group {
            append_group_terminology_hits(
                &mut output,
                &current_group_texts,
                current_group_unit_count,
                terminology,
                cancellation,
            )?;
            current_group_texts.clear();
            current_group_unit_count = 0;
            current_group_id = Some(group_id);
        } else if current_group_id.is_none() {
            current_group_id = Some(group_id);
        }

        let kind = sqlite_text_with_cancellation(
            row,
            1,
            "kind",
            "读取 Generic Group 术语命中事实失败",
            cancellation,
        )?;
        let source_text = sqlite_text_with_cancellation(
            row,
            2,
            "source_text",
            "读取 Generic Group 术语命中事实失败",
            cancellation,
        )?;
        ensure_generic_lua_running(cancellation)?;
        let protected = service
            .protect_with_cancellation(&kind, &source_text, placeholder_rules, || {
                ensure_generic_lua_running(cancellation)
            })?
            .map_err(generic_placeholder_error)?;
        ensure_generic_lua_running(cancellation)?;
        let language_text = protected
            .language_text_with_cancellation(|| ensure_generic_lua_running(cancellation))?
            .map_err(|source| {
                ProjectLuaCallError::new(
                    "placeholder",
                    format!("无法建立 Generic Group 术语扫描文本：{source}"),
                )
            })?;
        current_group_texts.push(language_text);
        current_group_unit_count += 1;
    }
    drop(rows);
    drop(statement);
    if current_group_id.is_some() {
        append_group_terminology_hits(
            &mut output,
            &current_group_texts,
            current_group_unit_count,
            terminology,
            cancellation,
        )?;
    }
    ensure_generic_lua_running(cancellation)?;
    Ok(output)
}

fn append_group_terminology_hits(
    output: &mut Vec<Sha256Fingerprint>,
    language_texts: &[LanguageText],
    unit_count: usize,
    terminology: &CompiledTerminology,
    cancellation: &CooperativeCancellation,
) -> Result<(), ProjectLuaCallError> {
    ensure_generic_lua_running(cancellation)?;
    let indices = terminology.triggered_indices_with_cancellation(
        language_texts.iter().flat_map(|text| {
            text.segments().iter().filter_map(|segment| match segment {
                LanguageTextSegment::NaturalText(text) => Some(text.as_str()),
                LanguageTextSegment::OpaqueBoundary => None,
            })
        }),
        || ensure_generic_lua_running(cancellation),
    )?;
    let fingerprint = terminology_hit_fingerprint_with_cancellation(terminology, &indices, || {
        ensure_generic_lua_running(cancellation)
    })?;
    for _ in 0..unit_count {
        ensure_generic_lua_running(cancellation)?;
        output.push(fingerprint);
    }
    ensure_generic_lua_running(cancellation)
}

fn capture_initial_translation_states(
    connection: &Connection,
    service: &GenericPlaceholderService,
    compiled: &GenericCompiledPlaceholderRules,
    group_terminology_hits: &[Sha256Fingerprint],
    cancellation: &CooperativeCancellation,
) -> Result<InitialTranslationStates, ProjectLuaCallError> {
    ensure_generic_lua_running(cancellation)?;
    let mut statement = connection
        .prepare(
            "SELECT generic_unit.group_id, generic_unit.unit_id,
                    generic_unit.translation,
                    generic_unit.translation_origin, generic_unit.translation_state,
                    generic_group.kind, generic_unit.source_text
             FROM main.generic_unit AS generic_unit
             JOIN main.generic_group AS generic_group USING (group_id)
             ORDER BY generic_group.relative_path, generic_group.ordinal,
                      generic_unit.ordinal",
        )
        .map_err(|source| {
            ProjectLuaCallError::new("sqlite", format!("准备捕获 Generic 译文状态失败：{source}"))
        })?;
    let mut rows = statement.query([]).map_err(|source| {
        ProjectLuaCallError::new("sqlite", format!("捕获 Generic 译文状态失败：{source}"))
    })?;
    let mut states = Vec::new();
    let mut unit_index = 0_usize;
    while let Some(row) = rows.next().map_err(|source| {
        ProjectLuaCallError::new("sqlite", format!("读取 Generic 译文状态失败：{source}"))
    })? {
        ensure_generic_lua_running(cancellation)?;
        let group_terminology_hit =
            group_terminology_hits
                .get(unit_index)
                .copied()
                .ok_or_else(|| {
                    ProjectLuaCallError::new(
                        "translation_state",
                        "脚本前 Generic Group 术语命中状态缺少对应 Unit",
                    )
                })?;
        unit_index += 1;
        let group_id = sqlite_text_with_cancellation(
            row,
            0,
            "group_id",
            "读取 Generic 译文状态失败",
            cancellation,
        )?;
        let unit_id = sqlite_text_with_cancellation(
            row,
            1,
            "unit_id",
            "读取 Generic 译文状态失败",
            cancellation,
        )?;
        let translation = sqlite_optional_text_with_cancellation(
            row,
            2,
            "translation",
            "读取 Generic 译文状态失败",
            cancellation,
        )?;
        let origin = sqlite_optional_text_with_cancellation(
            row,
            3,
            "translation_origin",
            "读取 Generic 译文状态失败",
            cancellation,
        )?;
        let state = sqlite_optional_blob_with_cancellation(
            row,
            4,
            "translation_state",
            "读取 Generic 译文状态失败",
            cancellation,
        )?;
        ensure_generic_lua_running(cancellation)?;
        let state = match (translation, origin, state) {
            (None, None, None) => None,
            (Some(translation), Some(origin), Some(state)) => {
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
                validate_translation_text(&translation, cancellation)?;
                let kind = sqlite_text_with_cancellation(
                    row,
                    5,
                    "kind",
                    "读取 Generic 译文状态失败",
                    cancellation,
                )?;
                let source_text = sqlite_text_with_cancellation(
                    row,
                    6,
                    "source_text",
                    "读取 Generic 译文状态失败",
                    cancellation,
                )?;
                ensure_generic_lua_running(cancellation)?;
                let protected = service
                    .protect_with_cancellation(&kind, &source_text, compiled, || {
                        ensure_generic_lua_running(cancellation)
                    })?
                    .map_err(generic_placeholder_error)?;
                ensure_generic_lua_running(cancellation)?;
                Some(InitialTranslationState {
                    origin,
                    state,
                    placeholder_binding: protected.binding_fingerprint(),
                    automatic_terminology_hits: (origin == TranslationOrigin::Automatic)
                        .then_some(group_terminology_hit),
                })
            }
            _ => {
                return Err(ProjectLuaCallError::new(
                    "translation_state",
                    "Generic 脚本前译文状态不完整",
                ));
            }
        };
        states.push(InitialTranslationEntry {
            group_id,
            unit_id,
            state,
        });
    }
    if unit_index != group_terminology_hits.len() {
        return Err(ProjectLuaCallError::new(
            "translation_state",
            "脚本前 Generic Group 术语命中状态与 Unit 数量不一致",
        ));
    }
    ensure_generic_lua_running(cancellation)?;
    Ok(states)
}

fn generic_text_eq_with_cancellation(
    left: &str,
    right: &str,
    cancellation: &CooperativeCancellation,
) -> Result<bool, ProjectLuaCallError> {
    ensure_generic_lua_running(cancellation)?;
    if left.len() != right.len() {
        return Ok(false);
    }
    for (left, right) in left
        .as_bytes()
        .chunks(CANCELLATION_TEXT_CHUNK_BYTES)
        .zip(right.as_bytes().chunks(CANCELLATION_TEXT_CHUNK_BYTES))
    {
        ensure_generic_lua_running(cancellation)?;
        if left != right {
            return Ok(false);
        }
    }
    ensure_generic_lua_running(cancellation)?;
    Ok(true)
}

fn generic_placeholder_error(error: GenericPlaceholderError) -> ProjectLuaCallError {
    match error {
        GenericPlaceholderError::Compilation(PlaceholderRuleCompilationError::StartWorker {
            operation,
            source,
        }) => ProjectLuaCallError::new(
            "worker_spawn",
            project_lua_worker_spawn_message(operation, &source),
        ),
        error => ProjectLuaCallError::new("placeholder", error.to_string()),
    }
}

fn sqlite_text_with_cancellation(
    row: &Row<'_>,
    index: usize,
    column: &'static str,
    operation: &'static str,
    cancellation: &CooperativeCancellation,
) -> Result<String, ProjectLuaCallError> {
    ensure_generic_lua_running(cancellation)?;
    match row
        .get_ref(index)
        .map_err(|source| sqlite_read_error(operation, source))?
    {
        ValueRef::Text(bytes) => {
            clone_sqlite_text_with_cancellation(bytes, index, operation, cancellation)
        }
        value => Err(sqlite_read_error(
            operation,
            rusqlite::Error::InvalidColumnType(index, column.to_owned(), value.data_type()),
        )),
    }
}

fn sqlite_optional_text_with_cancellation(
    row: &Row<'_>,
    index: usize,
    column: &'static str,
    operation: &'static str,
    cancellation: &CooperativeCancellation,
) -> Result<Option<String>, ProjectLuaCallError> {
    ensure_generic_lua_running(cancellation)?;
    match row
        .get_ref(index)
        .map_err(|source| sqlite_read_error(operation, source))?
    {
        ValueRef::Null => Ok(None),
        ValueRef::Text(bytes) => {
            clone_sqlite_text_with_cancellation(bytes, index, operation, cancellation).map(Some)
        }
        value => Err(sqlite_read_error(
            operation,
            rusqlite::Error::InvalidColumnType(index, column.to_owned(), value.data_type()),
        )),
    }
}

fn sqlite_optional_blob_with_cancellation(
    row: &Row<'_>,
    index: usize,
    column: &'static str,
    operation: &'static str,
    cancellation: &CooperativeCancellation,
) -> Result<Option<Vec<u8>>, ProjectLuaCallError> {
    ensure_generic_lua_running(cancellation)?;
    match row
        .get_ref(index)
        .map_err(|source| sqlite_read_error(operation, source))?
    {
        ValueRef::Null => Ok(None),
        ValueRef::Blob(bytes) => clone_sqlite_blob_with_cancellation(bytes, cancellation).map(Some),
        value => Err(sqlite_read_error(
            operation,
            rusqlite::Error::InvalidColumnType(index, column.to_owned(), value.data_type()),
        )),
    }
}

fn clone_sqlite_text_with_cancellation(
    bytes: &[u8],
    index: usize,
    operation: &'static str,
    cancellation: &CooperativeCancellation,
) -> Result<String, ProjectLuaCallError> {
    ensure_generic_lua_running(cancellation)?;
    let mut text = String::with_capacity(bytes.len());
    let mut pending = Vec::with_capacity(CANCELLATION_TEXT_CHUNK_BYTES + 3);
    for chunk in bytes.chunks(CANCELLATION_TEXT_CHUNK_BYTES) {
        ensure_generic_lua_running(cancellation)?;
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
                return Err(sqlite_read_error(
                    operation,
                    rusqlite::Error::FromSqlConversionFailure(index, Type::Text, Box::new(source)),
                ));
            }
        }
    }
    if !pending.is_empty() {
        let source = std::str::from_utf8(&pending).expect_err("pending 只保留不完整 UTF-8 后缀");
        return Err(sqlite_read_error(
            operation,
            rusqlite::Error::FromSqlConversionFailure(index, Type::Text, Box::new(source)),
        ));
    }
    ensure_generic_lua_running(cancellation)?;
    Ok(text)
}

fn clone_sqlite_blob_with_cancellation(
    bytes: &[u8],
    cancellation: &CooperativeCancellation,
) -> Result<Vec<u8>, ProjectLuaCallError> {
    ensure_generic_lua_running(cancellation)?;
    let mut cloned = Vec::with_capacity(bytes.len());
    for chunk in bytes.chunks(CANCELLATION_TEXT_CHUNK_BYTES) {
        ensure_generic_lua_running(cancellation)?;
        cloned.extend_from_slice(chunk);
    }
    ensure_generic_lua_running(cancellation)?;
    Ok(cloned)
}

fn sqlite_read_error(operation: &'static str, source: rusqlite::Error) -> ProjectLuaCallError {
    ProjectLuaCallError::new("sqlite", format!("{operation}：{source}"))
}

fn is_att_table(name: &str) -> bool {
    GENERIC_ATT_TABLES
        .iter()
        .any(|table| table.eq_ignore_ascii_case(name))
}

fn parse_locator(locator: ProjectLuaValue) -> Result<(String, String), ProjectLuaCallError> {
    let Some(mut fields) = locator.into_object() else {
        return Err(ProjectLuaCallError::new(
            "invalid_locator",
            "Generic locator 必须是只含 group_id 与 unit_id 的 table",
        ));
    };
    if fields.len() != 2
        || !project_lua_object_contains_field(&fields, "group_id")
        || !project_lua_object_contains_field(&fields, "unit_id")
    {
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
    fields: &mut Vec<(String, ProjectLuaValue)>,
    field: &'static str,
) -> Result<String, ProjectLuaCallError> {
    let Some(value) =
        take_project_lua_object_field(fields, field).and_then(ProjectLuaValue::into_text)
    else {
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

fn parse_translation(
    value: ProjectLuaValue,
    cancellation: &CooperativeCancellation,
) -> Result<String, ProjectLuaCallError> {
    let Some(value) = value.into_text() else {
        return Err(ProjectLuaCallError::new(
            "invalid_translation",
            "Generic translation 必须是 UTF-8 字符串",
        ));
    };
    validate_translation_text(&value, cancellation)?;
    Ok(value)
}

fn validate_translation_text(
    value: &str,
    cancellation: &CooperativeCancellation,
) -> Result<(), ProjectLuaCallError> {
    ensure_generic_lua_running(cancellation)?;
    let mut has_non_whitespace = false;
    let mut has_carriage_return = false;
    let mut has_nul = false;
    let mut bytes_since_check = 0;
    for character in value.chars() {
        bytes_since_check += character.len_utf8();
        if bytes_since_check >= CANCELLATION_TEXT_CHUNK_BYTES {
            ensure_generic_lua_running(cancellation)?;
            bytes_since_check = 0;
        }
        has_non_whitespace |= !character.is_whitespace();
        has_carriage_return |= character == '\r';
        has_nul |= character == '\0';
    }
    ensure_generic_lua_running(cancellation)?;
    if !has_non_whitespace {
        return Err(ProjectLuaCallError::new(
            "invalid_translation",
            "Generic translation 不能为空白",
        ));
    }
    if has_carriage_return {
        return Err(ProjectLuaCallError::new(
            "invalid_translation",
            "Generic translation 不能包含 CR（U+000D）",
        ));
    }
    if has_nul {
        return Err(ProjectLuaCallError::new(
            "invalid_translation",
            "Generic translation 不能包含 NUL（U+0000）",
        ));
    }
    Ok(())
}

fn ensure_generic_lua_running(
    cancellation: &CooperativeCancellation,
) -> Result<(), ProjectLuaCallError> {
    if cancellation.is_requested() {
        Err(ProjectLuaCallError::new(
            "cancelled",
            "Generic Lua 校验已取消",
        ))
    } else {
        Ok(())
    }
}

fn generic_prerequisite_error(error: GenericProjectError) -> ProjectLuaDatabasePrerequisiteError {
    match error {
        GenericProjectError::Cancelled => ProjectLuaDatabasePrerequisiteError::Cancelled,
        GenericProjectError::Sqlite { operation, source } => {
            ProjectLuaDatabasePrerequisiteError::sqlite(operation, &source)
        }
        error => ProjectLuaDatabasePrerequisiteError::invalid_project_state(error.to_string()),
    }
}

fn generic_error(error: crate::generic::GenericProjectError) -> ProjectLuaCallError {
    match error {
        GenericProjectError::Cancelled => {
            ProjectLuaCallError::new("cancelled", "Generic Lua 校验已取消")
        }
        GenericProjectError::WorkerStart { operation, source } => ProjectLuaCallError::new(
            "worker_spawn",
            project_lua_worker_spawn_message(operation, &source),
        ),
        error => ProjectLuaCallError::new("generic_project", error.to_string()),
    }
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

    fn project_with_jsonl(
        jsonl: &str,
    ) -> (tempfile::TempDir, GenericProjectStore, std::path::PathBuf) {
        let temporary = tempdir().expect("应建立临时目录");
        let source_root = temporary.path().join("source");
        fs::create_dir(&source_root).expect("应建立来源目录");
        fs::write(source_root.join("text.jsonl"), jsonl).expect("应写入 Generic JSONL");
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

    fn project() -> (tempfile::TempDir, GenericProjectStore, std::path::PathBuf) {
        project_with_jsonl(
            "{\"id\":\"opening\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"body\",\"text\":\"こんにちは {name}\"}]}\n",
        )
    }

    fn install_automatic_translation(database_path: &std::path::Path) {
        // ProjectLua 无法也不应重新解释 Translate 的 prompt/client/language 身份；现有
        // 32 字节 automatic state 是执行前受信事实，本组测试只验证脚本是否偷换该事实。
        Connection::open(database_path)
            .expect("应打开 Generic 数据库")
            .execute(
                "UPDATE main.generic_unit
                 SET translation = ?1,
                     translation_origin = 'automatic',
                     translation_state = ?2
                 WHERE group_id = 'opening' AND unit_id = 'body'",
                params![
                    "你好 {name}",
                    Sha256Fingerprint::from_bytes([7; 32]).as_bytes().as_slice()
                ],
            )
            .expect("应安装脚本前自动译文");
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
                generic_project_lua_adapter(expected_project, CooperativeCancellation::default()),
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
        let locator = ProjectLuaValue::Object(vec![
            ("group_id".to_owned(), ProjectLuaValue::Text(" ".to_owned())),
            ("unit_id".to_owned(), ProjectLuaValue::Text("\t".to_owned())),
        ]);

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
        let (translation_after, state_after): (String, Vec<u8>) = connection
            .query_row(
                "SELECT translation, translation_state FROM generic_unit
                 WHERE group_id = 'opening' AND unit_id = 'body'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("应读取修订后状态");
        assert_eq!(translation_after, "人工修订 {name}");
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
        let resource: String = Connection::open(&database_path)
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

        let invalid_placeholder = r#"[{"pattern":"("}]"#;
        let error = run(
            &database_path,
            &format!(
                r#"ctx.db.execute(
                     "UPDATE translation_resource SET canonical_json = ?1 " ..
                     "WHERE resource_kind = 'placeholder_rules'",
                     {{[=[{invalid_placeholder}]=]}}
                   )"#
            ),
        )
        .expect_err("JSON 合法但 PCRE2 无效的 Placeholder 资源不能提交");
        assert!(matches!(
            error,
            ProjectLuaRunError::RolledBack(ProjectLuaFailure::Host {
                operation: "translation.validate",
                ..
            })
        ));
        let placeholder: String = Connection::open(&database_path)
            .expect("应重开数据库")
            .query_row(
                "SELECT canonical_json FROM translation_resource
                 WHERE resource_kind = 'placeholder_rules'",
                [],
                |row| row.get(0),
            )
            .expect("应读取回滚后的 Placeholder 资源");
        assert_eq!(placeholder, r#"[{"pattern":"\\{[^}]+\\}"}]"#);

        for invalid_terminology in [
            "[1]",
            r#"[{"term":"同名","translation":"译文一","triggers":["触发一"]},{"term":"同名","translation":"译文二","triggers":["触发二"]}]"#,
        ] {
            let source = format!(
                r#"ctx.db.execute(
                     "UPDATE translation_resource SET canonical_json = ?1 " ..
                     "WHERE resource_kind = 'terminology'",
                     {{[=[{invalid_terminology}]=]}}
                   )"#
            );
            let error = run(&database_path, &source)
                .expect_err("类型或语义无效的 terminology 资源不能提交");
            assert!(matches!(
                error,
                ProjectLuaRunError::RolledBack(ProjectLuaFailure::Host {
                    operation: "translation.validate",
                    ..
                })
            ));
            let terminology: String = Connection::open(&database_path)
                .expect("应重开数据库")
                .query_row(
                    "SELECT canonical_json FROM translation_resource
                     WHERE resource_kind = 'terminology'",
                    [],
                    |row| row.get(0),
                )
                .expect("应读取回滚后的 terminology");
            assert_eq!(terminology, "[]");
        }
    }

    #[test]
    fn typed_set_uses_rules_selected_by_the_current_canonical_resource() {
        let (_temporary, _store, database_path) = project();
        run(
            &database_path,
            r#"
ctx.db.execute(
  "UPDATE translation_resource SET canonical_json = ?1 WHERE resource_kind = 'placeholder_rules'",
  {"[]"}
)
ctx.translation.set(
  {group_id = "opening", unit_id = "body"},
  "你好"
)
"#,
        )
        .expect("资源变化后 typed set 应使用当前空规则");

        let connection = Connection::open(database_path).expect("应重开数据库");
        let (resource, translation): (String, String) = connection
            .query_row(
                "SELECT translation_resource.canonical_json, generic_unit.translation
                 FROM translation_resource
                 CROSS JOIN generic_unit
                 WHERE translation_resource.resource_kind = 'placeholder_rules'
                   AND generic_unit.group_id = 'opening'
                   AND generic_unit.unit_id = 'body'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("应读取当前规则与译文");
        assert_eq!(resource, "[]");
        assert_eq!(translation, "你好");
    }

    #[test]
    fn final_validation_compares_actual_placeholder_binding_with_baseline() {
        let (_temporary, _store, database_path) = project();
        run(
            &database_path,
            r#"ctx.translation.set(
                 {group_id = "opening", unit_id = "body"},
                 "你好 {name}"
               )"#,
        )
        .expect("应先建立人工 Current");

        let changed_binding = run(
            &database_path,
            r#"ctx.db.execute(
                 "UPDATE translation_resource SET canonical_json = ?1 " ..
                 "WHERE resource_kind = 'placeholder_rules'",
                 {"[]"}
               )"#,
        )
        .expect_err("实际 Placeholder binding 改变时旧 state 不能继续使用");
        assert!(matches!(
            changed_binding,
            ProjectLuaRunError::RolledBack(ProjectLuaFailure::Host {
                operation: "translation.validate",
                ..
            })
        ));

        let equivalent_rules = r#"[{"pattern":"\\{[a-z]+\\}"}]"#;
        run(
            &database_path,
            &format!(
                r#"ctx.db.execute(
                     "UPDATE translation_resource SET canonical_json = ?1 " ..
                     "WHERE resource_kind = 'placeholder_rules'",
                     {{[=[{equivalent_rules}]=]}}
                   )"#
            ),
        )
        .expect("规则文本变化但该 Unit 实际 binding 不变时应允许提交");
        let stored: String = Connection::open(database_path)
            .expect("应重开数据库")
            .query_row(
                "SELECT canonical_json FROM translation_resource
                 WHERE resource_kind = 'placeholder_rules'",
                [],
                |row| row.get(0),
            )
            .expect("应读取新规则");
        assert_eq!(stored, equivalent_rules);
    }

    #[test]
    fn automatic_translation_rejects_relevant_terminology_change() {
        let (_temporary, _store, database_path) = project();
        install_automatic_translation(&database_path);
        let terminology = r#"[{"term":"挨拶","translation":"问候","triggers":["こんにちは"]}]"#;

        let error = run(
            &database_path,
            &format!(
                r#"ctx.db.execute(
                     "UPDATE translation_resource SET canonical_json = ?1 " ..
                     "WHERE resource_kind = 'terminology'",
                     {{[=[{terminology}]=]}}
                   )"#
            ),
        )
        .expect_err("自动译文所属 Group 的术语命中变化时必须回滚");
        assert!(matches!(
            error,
            ProjectLuaRunError::RolledBack(ProjectLuaFailure::Host {
                operation: "translation.validate",
                ..
            })
        ));
        let stored: String = Connection::open(database_path)
            .expect("应重开数据库")
            .query_row(
                "SELECT canonical_json FROM translation_resource
                 WHERE resource_kind = 'terminology'",
                [],
                |row| row.get(0),
            )
            .expect("应读取回滚后的术语");
        assert_eq!(stored, "[]");
    }

    #[test]
    fn automatic_translation_can_refine_text_while_preserving_semantic_state() {
        let (_temporary, _store, database_path) = project();
        install_automatic_translation(&database_path);
        let state_before: Vec<u8> = Connection::open(&database_path)
            .expect("应重开数据库")
            .query_row(
                "SELECT translation_state FROM main.generic_unit
                 WHERE group_id = 'opening' AND unit_id = 'body'",
                [],
                |row| row.get(0),
            )
            .expect("应读取自动状态");

        run(
            &database_path,
            r#"ctx.db.execute(
                 "UPDATE main.generic_unit SET translation = '自动修订 {name}' " ..
                 "WHERE group_id = 'opening' AND unit_id = 'body'"
               )"#,
        )
        .expect("已有 automatic Current 允许直接精修正文并保留语义状态");
        let (translation, state_after): (String, Vec<u8>) = Connection::open(database_path)
            .expect("应重开数据库")
            .query_row(
                "SELECT translation, translation_state FROM main.generic_unit
                 WHERE group_id = 'opening' AND unit_id = 'body'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("应读取精修后的自动译文");
        assert_eq!(translation, "自动修订 {name}");
        assert_eq!(state_after, state_before);
    }

    #[test]
    fn automatic_translation_rejects_terminology_change_triggered_by_untranslated_sibling() {
        let (_temporary, _store, database_path) = project_with_jsonl(
            "{\"id\":\"opening\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"body\",\"text\":\"こんにちは {name}\"},{\"id\":\"weather\",\"text\":\"今日は晴れ\"}]}\n",
        );
        install_automatic_translation(&database_path);
        let terminology = r#"[{"term":"晴れ","translation":"晴天","triggers":["晴れ"]}]"#;

        let error = run(
            &database_path,
            &format!(
                r#"ctx.db.execute(
                     "UPDATE translation_resource SET canonical_json = ?1 " ..
                     "WHERE resource_kind = 'terminology'",
                     {{[=[{terminology}]=]}}
                   )"#
            ),
        )
        .expect_err("未翻译 sibling 改变 Group 术语命中时旧自动状态必须回滚");
        assert!(matches!(
            error,
            ProjectLuaRunError::RolledBack(ProjectLuaFailure::Host {
                operation: "translation.validate",
                ..
            })
        ));
    }

    #[test]
    fn automatic_translation_allows_unrelated_terminology_change() {
        let (_temporary, _store, database_path) = project();
        install_automatic_translation(&database_path);
        let terminology = r#"[{"term":"天気","translation":"天气","triggers":["天気"]}]"#;

        run(
            &database_path,
            &format!(
                r#"ctx.db.execute(
                     "UPDATE translation_resource SET canonical_json = ?1 " ..
                     "WHERE resource_kind = 'terminology'",
                     {{[=[{terminology}]=]}}
                   )"#
            ),
        )
        .expect("没有命中当前 Group 的术语变化不应让自动译文失效");
        let stored: String = Connection::open(database_path)
            .expect("应重开数据库")
            .query_row(
                "SELECT canonical_json FROM translation_resource
                 WHERE resource_kind = 'terminology'",
                [],
                |row| row.get(0),
            )
            .expect("应读取已提交术语");
        assert_eq!(stored, terminology);
    }

    #[test]
    fn manual_translation_allows_relevant_terminology_change() {
        let (_temporary, _store, database_path) = project();
        run(
            &database_path,
            r#"ctx.translation.set(
                 {group_id = "opening", unit_id = "body"},
                 "你好 {name}"
               )"#,
        )
        .expect("应先建立人工 Current");
        let terminology = r#"[{"term":"挨拶","translation":"问候","triggers":["こんにちは"]}]"#;

        run(
            &database_path,
            &format!(
                r#"ctx.db.execute(
                     "UPDATE translation_resource SET canonical_json = ?1 " ..
                     "WHERE resource_kind = 'terminology'",
                     {{[=[{terminology}]=]}}
                   )"#
            ),
        )
        .expect("人工状态不依赖自动术语命中");
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
    fn schema_drift_is_rejected_before_the_lua_script_can_modify_the_database() {
        let (_temporary, store, database_path) = project();
        let expected_project = store.open().expect("schema 漂移前应能打开 Generic 项目");
        Connection::open(&database_path)
            .expect("应打开 Generic 数据库")
            .execute_batch(
                "ALTER TABLE translation_resource RENAME TO translation_resource_old;
                 CREATE TABLE translation_resource (
                     resource_kind TEXT PRIMARY KEY,
                     canonical_json TEXT NOT NULL
                 ) STRICT;
                 INSERT INTO translation_resource
                 SELECT * FROM translation_resource_old;
                 DROP TABLE translation_resource_old;",
            )
            .expect("应建立仍可读取但 DDL 已漂移的数据库");

        let connection =
            Connection::open_with_flags(&database_path, OpenFlags::SQLITE_OPEN_READ_WRITE)
                .expect("应绕过新的项目打开，模拟打开项目后才发生的 schema 漂移");
        let error = run_project_lua(
            connection,
            ProjectLuaRunRequest::new(
                ProjectLuaProject::new("game", "generic"),
                ProjectLuaProgram::new(
                    "generic.lua",
                    r#"ctx.db.execute("CREATE TABLE lua_marker (value TEXT)")"#.as_bytes(),
                    Vec::new(),
                ),
                generic_project_lua_adapter(expected_project, CooperativeCancellation::default()),
            ),
        )
        .expect_err("脚本前必须拒绝不是当前精确定义的 Generic schema");
        assert!(matches!(
            error,
            ProjectLuaRunError::RolledBack(ProjectLuaFailure::DatabasePrerequisite(
                ProjectLuaDatabasePrerequisiteError::InvalidProjectState(ref detail)
            )) if detail.contains("definition_mismatches=table/translation_resource")
        ));
        let marker_count: i64 = Connection::open(database_path)
            .expect("应重开 Generic 数据库")
            .query_row(
                "SELECT count(*) FROM sqlite_schema
                 WHERE type = 'table' AND name = 'lua_marker'",
                [],
                |row| row.get(0),
            )
            .expect("应检查脚本是否获得执行机会");
        assert_eq!(marker_count, 0);
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
