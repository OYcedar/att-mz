-- WriteBack：由 Managed metadata 定位冻结来源的完整 string Value，
-- 再把 Current 译文交给 Host 做原值比较、嵌套编码和重新读取验证。

assert(ctx.phase == "write_back")
assert(ctx.translations ~= nil and ctx.write_back ~= nil)

local collection = ctx.translations.open("quest_titles")
assert(collection ~= nil, "缺少 quest_titles Managed collection")

local document = ctx.rpg_maker.open(
  ctx.rpg_maker.data_file("QuestEntries.json")
)
local replacements = {}

for unit in collection:units() do
  assert(ctx.json.kind(unit.metadata) == "object")
  local json_index = unit.metadata.json_index
  local quest_id = unit.metadata.quest_id
  assert(math.type(json_index) == "integer" and json_index >= 0)
  assert(type(quest_id) == "string" and quest_id:match("%S"))

  local entry_id = document:value(ctx.json.array({ json_index, "id" }))
  assert(entry_id == quest_id, "metadata 指向了不同来源任务")

  local title = document:text(ctx.json.array({ json_index, "title" }))
  assert(title.original == unit.original, "Managed 原文与冻结 Value 不一致")

  if unit.status == "current" then
    assert(type(unit.translation) == "string")
    replacements[#replacements + 1] = {
      text = title,
      replacement = unit.translation,
    }
  else
    assert(unit.status == "missing")
  end
end

if #replacements > 0 then
  ctx.write_back.replace_text(replacements)
end
