assert(ctx.phase == "extract", "本示例只用于 Extract")

local source = ctx.rpg_maker.data_file("QuestEntries.json")
local document = ctx.rpg_maker.open(source)
local root = document:value(ctx.json.array())
assert(ctx.json.kind(root) == "array", "QuestEntries.json 根必须是数组")

local groups = {}
for lua_index, entry in ipairs(root) do
  if ctx.json.kind(entry) == "object" then
    local json_index = lua_index - 1
    local group_path = ctx.json.array({ json_index })
    local fields = {}

    -- fields 的声明顺序就是 unit_order；不要按字段名重排。
    if type(entry.title) == "string" and entry.title:match("%S") then
      fields[#fields + 1] = {
        name = "title",
        text = document:text(ctx.json.array({ json_index, "title" })),
      }
    end
    if type(entry.description) == "string" and entry.description:match("%S") then
      fields[#fields + 1] = {
        name = "description",
        text = document:text(ctx.json.array({ json_index, "description" })),
      }
    end

    if #fields > 0 then
      groups[#groups + 1] = {
        kind = "database_entry",
        location = document:location(group_path),
        fields = fields,
      }
    end
  end
end

-- groups 的声明顺序就是 group_order；空数组表示 active 的空快照。
ctx.extract.replace_standard(groups)
