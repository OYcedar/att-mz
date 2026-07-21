assert(ctx.phase == "write_back", "本示例只用于 WriteBack")
assert(ctx.write_back ~= nil, "WriteBack 阶段必须提供共享布局接口")

local path = "data/QuestEntries.json"
local entries = ctx.output.read_json(path)
assert(ctx.json.kind(entries) == "array", "QuestEntries.json 根必须是数组")

for lua_index, entry in ipairs(entries) do
  if ctx.json.kind(entry) == "object" and type(entry.id) == "string" then
    local identity = "quest:" .. entry.id .. ":title"
    local rows = ctx.db.query([[
SELECT original, translation
FROM lua_example_translation
WHERE identity = ?
]], { identity })
    if #rows == 1 then
      assert(entry.title == rows[1][1], "候选原文与私有协议漂移：" .. identity)
      entry.title = rows[1][2]
    end
  end
end

-- 候选每次从 source 重建；相同 source + 私有状态总得到相同 JSON。
ctx.output.write_json(path, entries)
-- 没有 post-publish hook，也不写“已发布”标志。
