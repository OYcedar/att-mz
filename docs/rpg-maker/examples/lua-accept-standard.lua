-- 一次性项目 Lua：把一条已经人工确认的译文交给 Standard 核心验收并提交。
-- 复制到真实项目时，必须同时替换完整身份、原文断言和候选。

assert(ctx.phase == "lua")
assert(ctx.standard ~= nil)
assert(ctx.extract == nil and ctx.translation == nil and ctx.llm == nil)
assert(ctx.output == nil and ctx.write_back == nil)
assert(type(arg[0]) == "string")

local standard = ctx.standard.open()
local items = ctx.rpg_maker.open(ctx.rpg_maker.data("Items.json"))
local expected_location = items:location({ 1 })
local target = nil

for unit in standard:units() do
  if unit.owner == "builtin"
     and unit.group_kind == "database_entry"
     and unit.group_location == expected_location
     and unit.role.kind == "scalar"
     and unit.role.field == "description"
     and unit.original == "药水" then
    assert(target == nil, "人工补译目标不唯一")
    target = unit
  end
end

assert(target ~= nil, "找不到人工补译目标")
assert(target.content_kind == "value")
assert(target.line_policy == "single")
assert(target.status == "missing" or target.status == "stale" or target.status == "current")

local results = standard:accept({
  {
    unit = target,
    candidate = "人工译文",
    replace_current = false,
  },
})

assert(#results == 1)
assert(results[1].accepted, results[1].reason)
assert(results[1].translation == "人工译文")
assert(results[1].changed_locations >= 0)
assert(results[1].changed_locations <= target.family_size)
