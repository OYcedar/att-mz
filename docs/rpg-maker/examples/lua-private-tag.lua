local function rollback_after(error_value)
  pcall(ctx.db.rollback)
  error(error_value, 0)
end

local function transaction(body)
  ctx.db.begin()
  local ok, failure = pcall(body)
  if not ok then
    rollback_after(failure)
  end
  local committed, commit_failure = pcall(ctx.db.commit)
  if not committed then
    rollback_after(commit_failure)
  end
end

local function create_schema()
  ctx.db.execute([[
CREATE TABLE IF NOT EXISTS lua_private_tag_unit (
  identity TEXT PRIMARY KEY,
  original TEXT NOT NULL,
  expected_value TEXT NOT NULL,
  semantic_context TEXT NOT NULL,
  translation TEXT,
  state TEXT,
  seen INTEGER NOT NULL DEFAULT 1 CHECK (seen IN (0, 1)),
  CHECK ((translation IS NULL) = (state IS NULL)),
  CHECK (state IS NULL OR (
    length(state) = 64 AND state NOT GLOB '*[^0-9a-f]*'
  ))
)
]])
end

-- 这是示例插件自己的 grammar；Host 不知道 Help、冒号或尖括号的含义。
local function parse_help_value(value)
  assert(type(value) == "string", "Help Value 必须是字符串")
  local text = string.match(value, "^<Help:([^<>]+)>$")
  assert(text ~= nil, "Help Value 不符合脚本私有 grammar")
  return text
end

local function render_help_value(text)
  assert(type(text) == "string" and text ~= "", "Help 译文必须是非空字符串")
  assert(not string.find(text, "[<>]"), "Help 译文会破坏脚本私有 grammar")
  local value = "<Help:" .. text .. ">"
  assert(parse_help_value(value) == text)
  return value
end

local identity = "item:1:help:0"
local semantic_context = "protocol=private-help-tag;surface=item-note"

local function extract_phase()
  create_schema()
  local document = ctx.rpg_maker.open(ctx.rpg_maker.data("Items.json"))
  local note = document:text(ctx.json.array({ 1, "note" })).original
  local original = parse_help_value(note)

  transaction(function()
    ctx.db.execute("UPDATE lua_private_tag_unit SET seen = 0")
    ctx.db.execute([[
INSERT INTO lua_private_tag_unit
  (identity, original, expected_value, semantic_context, translation, state, seen)
VALUES (?, ?, ?, ?, NULL, NULL, 1)
ON CONFLICT(identity) DO UPDATE SET
  translation = CASE
    WHEN original = excluded.original
     AND expected_value = excluded.expected_value
     AND semantic_context = excluded.semantic_context THEN translation
    ELSE NULL
  END,
  state = CASE
    WHEN original = excluded.original
     AND expected_value = excluded.expected_value
     AND semantic_context = excluded.semantic_context THEN state
    ELSE NULL
  END,
  original = excluded.original,
  expected_value = excluded.expected_value,
  semantic_context = excluded.semantic_context,
  seen = 1
]], { identity, original, note, semantic_context })
    ctx.db.execute("DELETE FROM lua_private_tag_unit WHERE seen = 0")
  end)
end

local function translate_phase()
  create_schema()
  local rows = ctx.db.query([[
SELECT original, semantic_context, translation, state
FROM lua_private_tag_unit
WHERE identity = ?
]], { identity })
  assert(#rows == 1, "必须先运行私有标签 Extract")

  local original, context = rows[1][1], rows[1][2]
  local translation = rows[1][3] ~= ctx.db.NULL and rows[1][3] or nil
  local state = rows[1][4] ~= ctx.db.NULL and rows[1][4] or nil
  local prepared = ctx.translation.prepare("database_entry", original, context)
  if translation ~= nil and state ~= nil
     and prepared:is_current(translation, state) then
    return
  end

  transaction(function()
    ctx.db.execute([[
UPDATE lua_private_tag_unit
SET translation = NULL, state = NULL
WHERE identity = ?
]], { identity })
  end)
  if prepared.status ~= "active" then
    return
  end

  local response = ctx.llm({
    { role = "system", content = ctx.translation.system_prompt },
    {
      role = "user",
      content = "只返回 Help 正文译文，不要返回标签外壳：\n" .. prepared.model_text,
    },
  })
  if response.finish_reason ~= "stop" then
    error("LLM 未正常结束：" .. response.finish_reason)
  end
  local accepted = prepared:accept(response.content)
  if not accepted.accepted then
    error("公共翻译验收失败：" .. accepted.reason)
  end

  -- 公共 accept 不知道插件 grammar；脚本必须在私有事务前自己验收。
  render_help_value(accepted.translation)
  transaction(function()
    ctx.db.execute([[
UPDATE lua_private_tag_unit
SET translation = ?, state = ?
WHERE identity = ? AND original = ? AND semantic_context = ?
]], {
      accepted.translation,
      accepted.state,
      identity,
      original,
      context,
    })
  end)
end

local function write_back_phase()
  create_schema()
  local path = "data/Items.json"
  local items = ctx.output.read_json(path)
  local item = items[2]
  assert(ctx.json.kind(item) == "object", "Items.json[1] 必须是对象")

  local rows = ctx.db.query([[
SELECT original, expected_value, translation
FROM lua_private_tag_unit
WHERE identity = ? AND translation IS NOT NULL AND state IS NOT NULL
]], { identity })
  if #rows == 1 then
    local original, expected_value, translation = rows[1][1], rows[1][2], rows[1][3]
    assert(item.note == expected_value, "候选完整 Help Value 已漂移")
    assert(parse_help_value(item.note) == original, "候选 Help 正文已漂移")
    item.note = render_help_value(translation)
  end

  ctx.output.write_json(path, items)
end

if ctx.phase == "extract" then
  extract_phase()
elseif ctx.phase == "translate" then
  translate_phase()
elseif ctx.phase == "write_back" then
  write_back_phase()
else
  error("不支持的阶段：" .. tostring(ctx.phase))
end
