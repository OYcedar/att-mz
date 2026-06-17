use super::command_writer::{
    render_mv_virtual_speaker_line_from_render_parts, write_command_first_parameter,
};
use super::models::{
    COMMON_EVENTS_FILE_NAME, EngineKind, Layout, MvVirtualNameboxFactTemplate,
    MvVirtualSpeakerPolicy, SYSTEM_FILE_NAME, TROOPS_FILE_NAME, TextFactRenderPart,
};
use super::utils::is_map_file;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};

pub(super) fn apply_terminology(
    data_files: &mut BTreeMap<String, Value>,
    terminology: &HashMap<String, HashMap<String, String>>,
    layout: &Layout,
) -> Result<usize, String> {
    let mut written_count = 0usize;
    if let Some(speaker_names) = terminology.get("speaker_names") {
        written_count += match layout.engine_kind {
            EngineKind::Mz => write_mz_speaker_names(data_files, speaker_names)?,
            EngineKind::Mv => 0,
        };
    }
    if let Some(map_names) = terminology.get("map_display_names") {
        for (file_name, value) in data_files.iter_mut() {
            if !is_map_file(file_name) {
                continue;
            }
            let object = value
                .as_object_mut()
                .ok_or_else(|| format!("{file_name} 顶层不是地图对象"))?;
            let Some(source_text) = object.get("displayName").and_then(Value::as_str) else {
                continue;
            };
            if let Some(translated_text) = map_names.get(source_text.trim()) {
                object.insert(
                    "displayName".to_string(),
                    Value::String(translated_text.clone()),
                );
                written_count += 1;
            }
        }
    }
    let base_categories = [
        ("Actors.json", "name", "actor_names"),
        ("Actors.json", "nickname", "actor_nicknames"),
        ("Classes.json", "name", "class_names"),
        ("Skills.json", "name", "skill_names"),
        ("Items.json", "name", "item_names"),
        ("Weapons.json", "name", "weapon_names"),
        ("Armors.json", "name", "armor_names"),
        ("Enemies.json", "name", "enemy_names"),
        ("States.json", "name", "state_names"),
    ];
    for (file_name, key, category) in base_categories {
        let Some(translations) = terminology.get(category) else {
            continue;
        };
        let values = data_files
            .get_mut(file_name)
            .ok_or_else(|| format!("字段译名目标文件不存在: {file_name}"))?
            .as_array_mut()
            .ok_or_else(|| format!("字段译名目标文件不是数组: {file_name}"))?;
        for value in values {
            if value.is_null() {
                continue;
            }
            let Some(object) = value.as_object_mut() else {
                return Err(format!("{file_name} 存在非对象条目，不能写入字段译名"));
            };
            let Some(source_text) = object.get(key).and_then(Value::as_str) else {
                continue;
            };
            if let Some(translated_text) = translations.get(source_text.trim()) {
                object.insert(key.to_string(), Value::String(translated_text.clone()));
                written_count += 1;
            }
        }
    }
    let system_categories = [
        ("elements", "system_elements"),
        ("skillTypes", "system_skill_types"),
        ("weaponTypes", "system_weapon_types"),
        ("armorTypes", "system_armor_types"),
        ("equipTypes", "system_equip_types"),
    ];
    let has_system_terms = system_categories
        .iter()
        .any(|(_field_name, category)| terminology.contains_key(*category));
    if !has_system_terms {
        return Ok(written_count);
    }
    let system = data_files
        .get_mut(SYSTEM_FILE_NAME)
        .ok_or_else(|| "字段译名目标文件不存在: System.json".to_string())?
        .as_object_mut()
        .ok_or_else(|| "System.json 顶层不是对象，不能写入系统字段译名".to_string())?;
    for (field_name, category) in system_categories {
        let Some(translations) = terminology.get(category) else {
            continue;
        };
        let values = system
            .get_mut(field_name)
            .and_then(Value::as_array_mut)
            .ok_or_else(|| format!("System.{field_name} 不是数组，不能写入系统字段译名"))?;
        for value in values {
            let Some(source_text) = value.as_str() else {
                continue;
            };
            if let Some(translated_text) = translations.get(source_text.trim()) {
                *value = Value::String(translated_text.clone());
                written_count += 1;
            }
        }
    }
    Ok(written_count)
}

pub(super) fn apply_mv_virtual_speaker_names(
    data_files: &mut BTreeMap<String, Value>,
    terminology: &HashMap<String, HashMap<String, String>>,
    mv_virtual_namebox_fact_templates: &[MvVirtualNameboxFactTemplate],
    skipped_location_paths: &BTreeSet<String>,
) -> Result<usize, String> {
    let Some(speaker_names) = terminology.get("speaker_names") else {
        return Ok(0);
    };
    write_mv_virtual_speaker_names(
        data_files,
        speaker_names,
        mv_virtual_namebox_fact_templates,
        skipped_location_paths,
    )
}

pub(super) fn write_mz_speaker_names(
    data_files: &mut BTreeMap<String, Value>,
    translations: &HashMap<String, String>,
) -> Result<usize, String> {
    let mut written_count = 0usize;
    for (file_name, value) in data_files.iter_mut() {
        if is_map_file(file_name) {
            written_count += write_mz_map_speaker_names(file_name, value, translations)?;
        }
    }
    if let Some(value) = data_files.get_mut(COMMON_EVENTS_FILE_NAME) {
        written_count += write_mz_common_event_speaker_names(value, translations)?;
    }
    if let Some(value) = data_files.get_mut(TROOPS_FILE_NAME) {
        written_count += write_mz_troop_speaker_names(value, translations)?;
    }
    Ok(written_count)
}

pub(super) fn write_mz_map_speaker_names(
    file_name: &str,
    value: &mut Value,
    translations: &HashMap<String, String>,
) -> Result<usize, String> {
    let object = value
        .as_object_mut()
        .ok_or_else(|| format!("{file_name} 顶层不是地图对象，不能写入名字框术语"))?;
    let events = object
        .get_mut("events")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| format!("{file_name}.events 不是数组，不能写入名字框术语"))?;
    let mut written_count = 0usize;
    for (event_index, event) in events.iter_mut().enumerate() {
        if event.is_null() {
            continue;
        }
        let event_context = format!("{file_name}/{event_index}");
        let event_object = event
            .as_object_mut()
            .ok_or_else(|| format!("{event_context} 不是事件对象，不能写入名字框术语"))?;
        let pages = event_object
            .get_mut("pages")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| format!("{event_context}.pages 不是数组，不能写入名字框术语"))?;
        for (page_index, page) in pages.iter_mut().enumerate() {
            let page_context = format!("{event_context}/{page_index}");
            let page_object = page
                .as_object_mut()
                .ok_or_else(|| format!("{page_context} 不是事件页对象，不能写入名字框术语"))?;
            let commands = page_object
                .get_mut("list")
                .and_then(Value::as_array_mut)
                .ok_or_else(|| format!("{page_context}.list 不是数组，不能写入名字框术语"))?;
            written_count +=
                write_mz_speaker_names_to_commands(commands, translations, &page_context)?;
        }
    }
    Ok(written_count)
}

pub(super) fn write_mz_common_event_speaker_names(
    value: &mut Value,
    translations: &HashMap<String, String>,
) -> Result<usize, String> {
    let events = value
        .as_array_mut()
        .ok_or_else(|| "CommonEvents.json 顶层不是数组，不能写入名字框术语".to_string())?;
    let mut written_count = 0usize;
    for (event_index, event) in events.iter_mut().enumerate() {
        if event.is_null() {
            continue;
        }
        let event_context = format!("{COMMON_EVENTS_FILE_NAME}/{event_index}");
        let event_object = event
            .as_object_mut()
            .ok_or_else(|| format!("{event_context} 不是公共事件对象，不能写入名字框术语"))?;
        let commands = event_object
            .get_mut("list")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| format!("{event_context}.list 不是数组，不能写入名字框术语"))?;
        written_count +=
            write_mz_speaker_names_to_commands(commands, translations, &event_context)?;
    }
    Ok(written_count)
}

pub(super) fn write_mz_troop_speaker_names(
    value: &mut Value,
    translations: &HashMap<String, String>,
) -> Result<usize, String> {
    let troops = value
        .as_array_mut()
        .ok_or_else(|| "Troops.json 顶层不是数组，不能写入名字框术语".to_string())?;
    let mut written_count = 0usize;
    for (troop_index, troop) in troops.iter_mut().enumerate() {
        if troop.is_null() {
            continue;
        }
        let troop_context = format!("{TROOPS_FILE_NAME}/{troop_index}");
        let troop_object = troop
            .as_object_mut()
            .ok_or_else(|| format!("{troop_context} 不是敌群对象，不能写入名字框术语"))?;
        let pages = troop_object
            .get_mut("pages")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| format!("{troop_context}.pages 不是数组，不能写入名字框术语"))?;
        for (page_index, page) in pages.iter_mut().enumerate() {
            let page_context = format!("{troop_context}/{page_index}");
            let page_object = page
                .as_object_mut()
                .ok_or_else(|| format!("{page_context} 不是敌群事件页对象，不能写入名字框术语"))?;
            let commands = page_object
                .get_mut("list")
                .and_then(Value::as_array_mut)
                .ok_or_else(|| format!("{page_context}.list 不是数组，不能写入名字框术语"))?;
            written_count +=
                write_mz_speaker_names_to_commands(commands, translations, &page_context)?;
        }
    }
    Ok(written_count)
}

pub(super) fn write_mz_speaker_names_to_commands(
    commands: &mut [Value],
    translations: &HashMap<String, String>,
    command_path_prefix: &str,
) -> Result<usize, String> {
    let mut written_count = 0usize;
    for (command_index, command_value) in commands.iter_mut().enumerate() {
        let command_path = format!("{command_path_prefix}/{command_index}");
        let command = command_value
            .as_object_mut()
            .ok_or_else(|| format!("{command_path} 不是事件指令对象，不能写入名字框术语"))?;
        let code = command
            .get("code")
            .and_then(Value::as_i64)
            .ok_or_else(|| format!("{command_path}.code 不是整数，不能写入名字框术语"))?;
        if code != 101 {
            continue;
        }
        let parameters = command
            .get_mut("parameters")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| format!("{command_path}.parameters 不是数组，不能写入名字框术语"))?;
        if parameters.len() <= 4 {
            continue;
        }
        let source_text = parameters[4]
            .as_str()
            .ok_or_else(|| format!("{command_path}.parameters[4] 不是文本，不能写入名字框术语"))?
            .trim();
        if let Some(translated_text) = translations.get(source_text) {
            parameters[4] = Value::String(translated_text.clone());
            written_count += 1;
        }
    }
    Ok(written_count)
}

fn write_mv_virtual_speaker_names(
    data_files: &mut BTreeMap<String, Value>,
    translations: &HashMap<String, String>,
    mv_virtual_namebox_fact_templates: &[MvVirtualNameboxFactTemplate],
    skipped_location_paths: &BTreeSet<String>,
) -> Result<usize, String> {
    let targets = collect_mv_virtual_speaker_name_writes(
        translations,
        mv_virtual_namebox_fact_templates,
        skipped_location_paths,
    )?;
    let written_count = targets.len();
    for (target_path, translated_text) in targets {
        write_command_first_parameter(data_files, &target_path, 401, &translated_text)?;
    }
    Ok(written_count)
}

fn collect_mv_virtual_speaker_name_writes(
    translations: &HashMap<String, String>,
    mv_virtual_namebox_fact_templates: &[MvVirtualNameboxFactTemplate],
    skipped_location_paths: &BTreeSet<String>,
) -> Result<Vec<(String, String)>, String> {
    // 直接遍历当前文本事实模板：每个模板的 location_path 即 101 命令路径，
    // source_line_paths 首元素即名字框说话人行路径，role 即术语查表键，
    // speaker_policy/source_speaker/render_parts 已由索引阶段一次性计算。
    // 不再重新扫描 data_files 的 401 指令解析说话人。
    let mut targets = Vec::new();
    for fact_template in mv_virtual_namebox_fact_templates {
        // location_path 是 101 命令路径；已被翻译项覆盖的对话块由命令项写回处理，
        // 术语写回只负责没有翻译项的名字框块（避免串位）。
        if skipped_location_paths.contains(&fact_template.location_path) {
            continue;
        }
        if matches!(
            fact_template.speaker_policy,
            MvVirtualSpeakerPolicy::Preserve
        ) {
            continue;
        }
        let Some(translated_speaker) = translations.get(&fact_template.role) else {
            continue;
        };
        let Some(speaker_line_path) = fact_template.source_line_paths.first() else {
            return Err(format!(
                "MV 虚拟名字框当前文本事实缺少说话人行路径，请重新运行 rebuild-text-index: {}",
                fact_template.location_path
            ));
        };
        // 术语写回只替换说话人，保留原文 body。内联名字框（speaker 行含 body）需要把
        // 该行原文 body 一起渲染回 speaker 行；独立名字框（body 在后续 401 行）没有内联
        // body，传 None 只写说话人。内联 body 是 speaker 片段之后、第一个含换行 literal
        // 之前的 body 片段；若 speaker 后先遇到换行 literal 则是独立名字框。
        let speaker_line_body = mv_inline_speaker_body(&fact_template.render_parts);
        let translated_text = render_mv_virtual_speaker_line_from_fact_template(
            fact_template,
            translated_speaker,
            speaker_line_body,
        )?;
        targets.push((speaker_line_path.clone(), translated_text));
    }
    Ok(targets)
}

/// 返回名字框 speaker 行内联的原文 body：speaker 片段之后、第一个含换行 literal
/// 之前的 body 片段 raw_text。独立名字框（speaker 后先遇换行 literal）返回 None。
fn mv_inline_speaker_body(render_parts: &[TextFactRenderPart]) -> Option<&str> {
    let mut after_speaker = false;
    for part in render_parts {
        if part.part_kind == "speaker" {
            after_speaker = true;
            continue;
        }
        if !after_speaker {
            continue;
        }
        if part.part_kind == "translated_body" || part.template_key == "body" {
            return Some(&part.raw_text);
        }
        if part.raw_text.contains('\n') || part.raw_text.contains('\r') {
            return None;
        }
    }
    None
}

fn render_mv_virtual_speaker_line_from_fact_template(
    fact_template: &MvVirtualNameboxFactTemplate,
    translated_speaker: &str,
    translated_body: Option<&str>,
) -> Result<String, String> {
    if fact_template.render_parts.is_empty() {
        return Err(format!(
            "MV 虚拟名字框当前文本事实缺少写回所需源文结构，不能写入 speaker_names；请重新运行 rebuild-text-index: {}",
            fact_template.location_path
        ));
    }
    if !fact_template
        .render_parts
        .iter()
        .any(|part| part.part_kind == "speaker")
    {
        return Err(format!(
            "MV 虚拟名字框当前文本事实缺少说话人片段，不能写入 speaker_names；请重新运行 rebuild-text-index: {}",
            fact_template.location_path
        ));
    }
    render_mv_virtual_speaker_line_from_render_parts(
        &fact_template.location_path,
        &fact_template.role,
        &fact_template.render_parts,
        translated_speaker,
        translated_body,
    )
}
