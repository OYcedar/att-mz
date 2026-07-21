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
CREATE TABLE IF NOT EXISTS lua_complex_unit (
  identity TEXT PRIMARY KEY,
  original TEXT NOT NULL,
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
  ctx.db.execute([[
CREATE TABLE IF NOT EXISTS lua_complex_target (
  identity TEXT NOT NULL,
  target_order INTEGER NOT NULL CHECK (target_order >= 0),
  document_name TEXT NOT NULL,
  json_index INTEGER,
  object_key TEXT,
  field_name TEXT NOT NULL,
  expected_original TEXT NOT NULL,
  PRIMARY KEY (identity, target_order),
  FOREIGN KEY (identity) REFERENCES lua_complex_unit(identity) ON DELETE CASCADE
)
]])
end

local function extract_phase()
  create_schema()
  local graph_document = ctx.rpg_maker.open(
    ctx.rpg_maker.data_file("QuestGraph.json")
  )
  local graph = graph_document:value(ctx.json.array())
  local index_document = ctx.rpg_maker.open(
    ctx.rpg_maker.data_file("QuestIndex.json")
  )
  local index = index_document:value(ctx.json.array())
  local actors = ctx.rpg_maker.open(ctx.rpg_maker.data("Actors.json"))

  transaction(function()
    ctx.db.execute("UPDATE lua_complex_unit SET seen = 0")
    ctx.db.execute("DELETE FROM lua_complex_target")

    for lua_index, quest in ipairs(graph) do
      assert(ctx.json.kind(quest) == "object", "QuestGraph 元素必须是对象")
      assert(type(quest.id) == "string" and type(quest.title) == "string")
      assert(type(quest.actorId) == "number" and type(quest.mapId) == "number")

      local actor = actors:value(ctx.json.array({ quest.actorId }))
      local map = ctx.rpg_maker.open(ctx.rpg_maker.map(quest.mapId))
      local map_name = map:value(ctx.json.array({ "displayName" }))
      assert(type(actor.name) == "string" and type(map_name) == "string")

      local mirror = index[quest.id]
      assert(ctx.json.kind(mirror) == "object" and mirror.label == quest.title,
        "QuestIndex 多目标原文必须与 QuestGraph 一致")

      local identity = "quest:" .. quest.id
      local semantic_context = ctx.json.encode(ctx.json.object({
        actor = actor.name,
        map = map_name,
        protocol = "quest-title",
      }))

      ctx.db.execute([[
INSERT INTO lua_complex_unit
  (identity, original, semantic_context, translation, state, seen)
VALUES (?, ?, ?, NULL, NULL, 1)
ON CONFLICT(identity) DO UPDATE SET
  translation = CASE
    WHEN original = excluded.original
     AND semantic_context = excluded.semantic_context THEN translation
    ELSE NULL
  END,
  state = CASE
    WHEN original = excluded.original
     AND semantic_context = excluded.semantic_context THEN state
    ELSE NULL
  END,
  original = excluded.original,
  semantic_context = excluded.semantic_context,
  seen = 1
]], { identity, quest.title, semantic_context })

      ctx.db.execute([[
INSERT INTO lua_complex_target
  (identity, target_order, document_name, json_index, object_key,
   field_name, expected_original)
VALUES (?, 0, 'QuestGraph.json', ?, NULL, 'title', ?)
]], { identity, lua_index - 1, quest.title })
      ctx.db.execute([[
INSERT INTO lua_complex_target
  (identity, target_order, document_name, json_index, object_key,
   field_name, expected_original)
VALUES (?, 1, 'QuestIndex.json', NULL, ?, 'label', ?)
]], { identity, quest.id, quest.title })
    end

    ctx.db.execute("DELETE FROM lua_complex_unit WHERE seen = 0")
  end)
end

local function translate_phase()
  create_schema()
  local rows = ctx.db.query([[
SELECT identity, original, semantic_context, translation, state
FROM lua_complex_unit
ORDER BY identity
]])

  for _, row in ipairs(rows) do
    local identity, original, semantic_context = row[1], row[2], row[3]
    local translation = row[4] ~= ctx.db.NULL and row[4] or nil
    local state = row[5] ~= ctx.db.NULL and row[5] or nil
    local prepared = ctx.translation.prepare(
      "plugin_parameter", original, semantic_context
    )

    local current = translation ~= nil and state ~= nil
      and prepared:is_current(translation, state)
    if not current then
      -- 先清除陈旧 pair；后续外部请求失败也不能让 WriteBack 消费旧语义。
      transaction(function()
        ctx.db.execute([[
UPDATE lua_complex_unit
SET translation = NULL, state = NULL
WHERE identity = ?
]], { identity })
      end)
      if prepared.status == "active" then
        local response = ctx.llm({
          { role = "system", content = ctx.translation.system_prompt },
          {
            role = "user",
            content = "上下文：" .. semantic_context
              .. "\n只返回任务标题译文：\n" .. prepared.model_text,
          },
        })
        if response.finish_reason ~= "stop" then
          error("LLM 未正常结束：" .. response.finish_reason)
        end
        local accepted = prepared:accept(response.content)
        if not accepted.accepted then
          error(identity .. " 验收失败：" .. accepted.reason)
        end
        transaction(function()
          ctx.db.execute([[
UPDATE lua_complex_unit
SET translation = ?, state = ?
WHERE identity = ? AND original = ? AND semantic_context = ?
]], {
            accepted.translation,
            accepted.state,
            identity,
            original,
            semantic_context,
          })
        end)
      end
    end
  end
end

local function write_back_phase()
  create_schema()
  assert(ctx.write_back ~= nil, "WriteBack 阶段必须提供共享布局接口")
  local graph_path = "data/QuestGraph.json"
  local index_path = "data/QuestIndex.json"
  local graph = ctx.output.read_json(graph_path)
  local index = ctx.output.read_json(index_path)

  local rows = ctx.db.query([[
SELECT u.identity, u.translation, t.target_order, t.document_name,
       t.json_index, t.object_key, t.field_name, t.expected_original
FROM lua_complex_unit AS u
JOIN lua_complex_target AS t ON t.identity = u.identity
WHERE u.translation IS NOT NULL AND u.state IS NOT NULL
ORDER BY u.identity, t.target_order
]])

  for _, row in ipairs(rows) do
    local identity, translation, document_name = row[1], row[2], row[4]
    local expected, field = row[8], row[7]
    if document_name == "QuestGraph.json" then
      local value = graph[row[5] + 1]
      assert(value[field] == expected, "候选漂移：" .. identity .. ":graph")
      value[field] = translation
    elseif document_name == "QuestIndex.json" then
      local value = index[row[6]]
      assert(value[field] == expected, "候选漂移：" .. identity .. ":index")
      value[field] = translation
    else
      error("未知私有目标文档：" .. document_name)
    end
  end

  -- 同一候选和私有状态每次重建同一两个文件；不需要发布后标志。
  ctx.output.write_json(graph_path, graph)
  ctx.output.write_json(index_path, index)
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
