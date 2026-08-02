local sql

if ctx.project.engine == "generic" then
  sql = [=[
    SELECT json_object(
      'format', 'att-unit-review-v1',
      'engine', 'generic',
      'project', ?1,
      'unit_count', count(*),
      'units', json_group_array(
        json(unit_json)
        ORDER BY file_ordinal, group_ordinal, unit_ordinal
      )
    )
    FROM (
      SELECT json_object(
        'kind', generic_group.kind,
        'group_id', generic_unit.group_id,
        'unit_id', generic_unit.unit_id,
        'source', generic_unit.source_text,
        'translation', generic_unit.translation,
        'translation_origin', generic_unit.translation_origin
      ) AS unit_json,
      generic_file.ordinal AS file_ordinal,
      generic_group.ordinal AS group_ordinal,
      generic_unit.ordinal AS unit_ordinal
      FROM main.generic_file AS generic_file
      JOIN main.generic_group AS generic_group
        ON generic_group.relative_path = generic_file.relative_path
      JOIN main.generic_unit AS generic_unit
        ON generic_unit.group_id = generic_group.group_id
      ORDER BY generic_file.ordinal,
               generic_group.ordinal,
               generic_unit.ordinal
    )
  ]=]
else
  sql = [=[
    SELECT json_object(
      'format', 'att-unit-review-v1',
      'engine', ?1,
      'project', ?2,
      'unit_count', count(*),
      'units', json_group_array(
        json(unit_json)
        ORDER BY group_order_key, unit_order_key, owner_order
      )
    )
    FROM (
      SELECT json_object(
        'kind', text_group.group_kind,
        'owner', text_unit.owner,
        'group_location', text_unit.group_location,
        'unit_role', text_unit.unit_role,
        'source_content_json', text_unit.source_content_json,
        'source_context_json', text_unit.source_context_json,
        'translation_content_json', text_unit.translation_content_json
      ) AS unit_json,
      text_group.semantic_order_key AS group_order_key,
      text_unit.semantic_order_key AS unit_order_key,
      CASE text_unit.owner WHEN 'builtin' THEN 0 ELSE 1 END AS owner_order
      FROM main.rpg_maker_text_group AS text_group
      JOIN main.rpg_maker_text_unit AS text_unit
        ON text_unit.owner = text_group.owner
       AND text_unit.group_location = text_group.group_location
      ORDER BY text_group.semantic_order_key,
               text_unit.semantic_order_key,
               CASE text_unit.owner WHEN 'builtin' THEN 0 ELSE 1 END
    )
  ]=]
end

local parameters
if ctx.project.engine == "generic" then
  parameters = { ctx.project.name }
else
  parameters = { ctx.project.engine, ctx.project.name }
end

local rows = ctx.db.query(sql, parameters)
assert(#rows == 1 and #rows[1] == 1, "审查查询必须只返回一份 JSON")
local payload = rows[1][1]
local hex = payload:gsub(".", function(byte)
  return string.format("%02x", string.byte(byte))
end)
print("att-unit-review-v1-hex:" .. #payload .. ":" .. hex)
