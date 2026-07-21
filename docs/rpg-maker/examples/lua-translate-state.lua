assert(ctx.phase == "translate", "本示例只用于 Translate")

ctx.db.execute([[
CREATE TABLE IF NOT EXISTS lua_example_translation (
  identity TEXT PRIMARY KEY,
  original TEXT NOT NULL,
  semantic_context TEXT NOT NULL,
  translation TEXT NOT NULL,
  state TEXT NOT NULL CHECK (
    length(state) = 64 AND state NOT GLOB '*[^0-9a-f]*'
  )
)
]])

local function rollback_after(error_value)
  pcall(ctx.db.rollback)
  error(error_value, 0)
end

local function save(identity, original, semantic_context, translation, state)
  ctx.db.begin()
  local ok, failure = pcall(function()
    ctx.db.execute([[
INSERT INTO lua_example_translation
  (identity, original, semantic_context, translation, state)
VALUES (?, ?, ?, ?, ?)
ON CONFLICT(identity) DO UPDATE SET
  original = excluded.original,
  semantic_context = excluded.semantic_context,
  translation = excluded.translation,
  state = excluded.state
]], { identity, original, semantic_context, translation, state })
    ctx.db.commit()
  end)
  if not ok then
    rollback_after(failure)
  end
end

local function clear_stale(identity)
  ctx.db.begin()
  local ok, failure = pcall(function()
    ctx.db.execute(
      "DELETE FROM lua_example_translation WHERE identity = ?",
      { identity }
    )
    ctx.db.commit()
  end)
  if not ok then
    rollback_after(failure)
  end
end

local identity = "quest:arrival:title"
local original = "星港へ"
-- 修改这里的协议事实会使旧 state 失效；没有私有语义时应传 ""。
local semantic_context = "protocol=quest-title;surface=menu"
local prepared = ctx.translation.prepare("database_entry", original, semantic_context)

local rows = ctx.db.query([[
SELECT translation, state
FROM lua_example_translation
WHERE identity = ? AND original = ? AND semantic_context = ?
]], { identity, original, semantic_context })

if #rows == 1 and prepared:is_current(rows[1][1], rows[1][2]) then
  -- 二次运行走这里，不调用 LLM。
  return
end

-- 旧 pair 不再 Current 时先成对移除。即使后续 LLM 失败，WriteBack 也不会消费陈旧译文。
clear_stale(identity)

if prepared.status ~= "active" then
  return
end

local response = ctx.llm({
  { role = "system", content = ctx.translation.system_prompt },
  {
    role = "user",
    content = "只返回译文，不要解释：\n" .. prepared.model_text,
  },
})
if response.finish_reason ~= "stop" then
  error("LLM 未正常结束：" .. response.finish_reason)
end

local accepted = prepared:accept(response.content)
if not accepted.accepted then
  error("译文未通过标量验收：" .. accepted.reason)
end

-- translation/state 在同一事务中成对写入脚本私有表。
save(identity, original, semantic_context, accepted.translation, accepted.state)
