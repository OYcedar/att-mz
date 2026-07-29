local collection_name = "quest_titles"
local data_file_name = "QuestEntries.json"

local function read_source_entries()
  local document = ctx.rpg_maker.open(
    ctx.rpg_maker.data_file(data_file_name)
  )
  local entries = document:value(ctx.json.array())
  assert(ctx.json.kind(entries) == "array", "QuestEntries.json 根必须是数组")
  return entries
end

local function inspect_entry(entry)
  assert(ctx.json.kind(entry) == "object", "QuestEntries.json 每一项必须是对象")
  assert(type(entry.id) == "string" and entry.id:match("%S"), "任务 id 必须是非空字符串")
  assert(
    type(entry.title) == "string" and entry.title:match("%S"),
    "任务标题必须是非空字符串"
  )
  assert(
    entry.description == nil or type(entry.description) == "string",
    "任务说明必须是字符串或不存在"
  )
  return entry.id, entry.title, entry.description or ""
end

if ctx.phase == "extract" then
  local entries = read_source_entries()
  local units = {}
  for lua_index, entry in ipairs(entries) do
    local quest_id, title, description = inspect_entry(entry)

    units[#units + 1] = {
      key = "quest:" .. quest_id,
      kind = "database_entry",
      shape = "single",
      original = title,
      context = "任务标题；相关说明：" .. description,
      metadata = ctx.json.object({
        json_index = lua_index - 1,
        quest_id = quest_id,
      }),
    }
  end

  ctx.translations.replace({
    {
      name = collection_name,
      instruction = "翻译任务标题；保持简洁，并结合任务说明判断含义。",
      units = units,
    },
  })
  return
end

if ctx.phase == "translate" then
  local report = ctx.translations.translate()
  for result in report:units() do
    print(result.collection, result.key, result.status, result.reason or "")
  end
  return
end

if ctx.phase == "write_back" then
  local collection = ctx.translations.open(collection_name)
  assert(collection ~= nil, "缺少 quest_titles Managed collection")

  local document = ctx.rpg_maker.open(
    ctx.rpg_maker.data_file(data_file_name)
  )
  local replacements = {}

  for unit in collection:units() do
    assert(ctx.json.kind(unit.metadata) == "object", "Managed metadata 必须是对象")
    local json_index = unit.metadata.json_index
    local quest_id = unit.metadata.quest_id
    assert(math.type(json_index) == "integer" and json_index >= 0, "json_index 必须是非负整数")
    assert(type(quest_id) == "string" and quest_id:match("%S"), "quest_id 必须是非空字符串")

    local source_id = document:value(ctx.json.array({ json_index, "id" }))
    local title = document:text(ctx.json.array({ json_index, "title" }))
    assert(source_id == quest_id, "metadata 指向了不同来源任务")
    assert(title.original == unit.original, "冻结来源标题与 Managed 原文不一致")

    if unit.status == "current" then
      assert(type(unit.translation) == "string", "Current unit 必须有标量译文")
      replacements[#replacements + 1] = {
        text = title,
        replacement = unit.translation,
      }
    else
      assert(unit.status == "missing", "WriteBack open 只应返回 current 或 missing")
    end
  end

  if #replacements > 0 then
    ctx.write_back.replace_text(replacements)
  end
  return
end

error("本示例只用于 Extract、Translate 或 WriteBack")
