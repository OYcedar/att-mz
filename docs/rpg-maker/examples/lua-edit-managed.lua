-- 一次性项目 Lua：同一脚本既可补齐 missing/stale，也可显式修订 Current。
-- 复制到真实项目时，必须替换 collection、key、原文断言和人工候选。

assert(ctx.phase == "lua")
assert(ctx.standard ~= nil and ctx.translations ~= nil)
assert(ctx.translation == nil and ctx.llm == nil)
assert(ctx.output == nil and ctx.write_back == nil)

local session = ctx.translations.edit()
local target = session:get("quest_titles", "quest:arrival")
assert(target ~= nil, "找不到 Managed 人工修订目标")
assert(target.kind == "database_entry")
assert(target.shape == "single")
assert(target.original == "星港へ")
assert(target.status == "missing"
    or target.status == "stale"
    or target.status == "current")

local results = session:accept({
  {
    unit = target,
    candidate = "抵达星港",
    -- 改变 Current 必须显式允许；missing/stale 不需要。
    replace_current = target.status == "current",
  },
})

assert(#results == 1)
assert(results[1].accepted, results[1].reason)
assert(results[1].translation == "抵达星港")
assert(results[1].changed_units >= 0)
assert(results[1].changed_units <= target.family_size)

-- 成功返回即已提交；重新查询当前会话得到更新后的权威投影。
local current = session:get("quest_titles", "quest:arrival")
assert(current ~= nil)
assert(current.status == "current")
assert(current.translation == "抵达星港")
