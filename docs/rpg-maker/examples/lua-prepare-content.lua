-- Translate 低级 Lua：复用公共结构、Placeholder、语言验收与 Current state，
-- identity、模型协议和持久化仍由调用方负责。

assert(ctx.phase == "translate")
assert(ctx.translation ~= nil)

local cases = {
  {
    shape = "single",
    original = "星港へ",
    candidate = "前往星港",
    expected_parts = 1,
  },
  {
    shape = "reflow",
    original = "星港へ向かう。\n夜明けまでに着け。",
    candidate = "前往星港。\n务必在天亮前抵达。",
    expected_parts = 1,
  },
  {
    shape = "lines",
    original = { "星港へ向かう。", "", "夜明けまでに着け。" },
    candidate = { "前往星港。", "", "务必在天亮前抵达。" },
    expected_parts = 1,
  },
  {
    shape = "items",
    original = { "受ける", "断る" },
    candidate = { "接受", "拒绝" },
    expected_parts = 2,
  },
}

for _, example in ipairs(cases) do
  local prepared = ctx.translation.prepare_content({
    kind = "database_entry",
    shape = example.shape,
    original = example.original,
    semantic_context = "surface=quest-menu",
  })

  assert(prepared.shape == example.shape)
  assert(prepared.status == "active")
  assert(#prepared.part_statuses == example.expected_parts)
  assert(type(prepared.terms) == "table")

  if example.shape == "single" or example.shape == "reflow" then
    assert(type(prepared.model_content) == "string")
  else
    assert(type(prepared.model_content) == "table")
    assert(#prepared.model_content == #example.original)
  end

  local accepted = prepared:accept(example.candidate)
  assert(accepted.accepted, accepted.reason)
  assert(prepared:is_current(accepted.translation, accepted.state))
end
