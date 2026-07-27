# RPG Maker Lua Cookbook

本页从“外部作者一次写对”出发，给出六种完整模式。所有可执行主程序位于
[`examples/`](examples/README.md)，测试会原样交给真实 Lua VM、临时 SQLite、冻结夹具
和假 LLM；本页的短代码只解释关键不变量。

代码块分类沿用 [Lua 技术参考](lua.md)的 `att-example` 标记。

声明式文件也提供生产解析器直接读取的完整样本：
[MV 姓名](examples/mv-dialogue.toml)、
[Extract Rules](examples/extract-rules.toml)、
[Placeholder](examples/placeholders.toml)、
[Terminology](examples/terminology.toml)。它们的字段含义分别以
[规则文件](rules.md)和[术语文件](terminology.md)为准。

## 1. 自定义 DataFile 标量接入 Standard

适用条件：每个语义字段就是一个完整 Value，且 source×kind×location
满足 [replace_standard 矩阵](lua.md#6-extractreplace_standard)。跨文档、多目标不要硬塞进
这个接口，直接跳到第 6 节；需要自行解释插件标签 grammar 时参见第 5 节。

完整脚本：[lua-standard-data-file.lua](examples/lua-standard-data-file.lua)。它：

1. 用 `data_file("QuestEntries.json")` 建立精确自定义来源；
2. 用 `document:value` 深拷贝枚举数组，但用 `document:text/location` 建立不可伪造引用；
3. 保持来源数组顺序和 `title → description` 声明顺序；
4. 恰好调用一次 `replace_standard`。

<!-- att-example: valid -->
```lua
assert(ctx.phase == "extract")
local document = ctx.rpg_maker.open(
  ctx.rpg_maker.data_file("QuestEntries.json")
)
ctx.extract.replace_standard({
  {
    kind = "database_entry",
    location = document:location(ctx.json.array({ 0 })),
    fields = {
      {
        name = "title",
        text = document:text(ctx.json.array({ 0, "title" })),
      },
    },
  },
})
```

若文件实际叫 `questentries.json`，应以真实目录项为权威并同步修改脚本；大小写不一致会
显式失败。`Map000.json` 可由 `data_file` 打开，但它是自定义
DataFile，不是 Map 0。

## 2. Translate state：首跑、复用和失效

完整脚本：[lua-translate-state.lua](examples/lua-translate-state.lua)。其私有表只保存已经
accept 的 translation/state，因此二者天然成对。

执行路线：

<!-- att-example: illustrative -->
```text
prepare(kind, original, semantic_context)
       |
       +-- 私有 translation/state 都存在
       |       |
       |       +-- is_current == true -> 直接复用，零 LLM
       |       `-- false -------------> 请求 LLM
       `-- 不完整/不存在 ------------> 请求 LLM
                                           |
                                           v
                                   accept(candidate)
                                           |
                       accepted -> 同一事务写 translation/state
                       rejected -> 陈旧 pair 已成对清除，不伪造成功
```

`semantic_context` 是脚本对 state 的承诺，不是给人看的备注。任何会改变正确译文、但 Host
不掌握的事实都必须稳定编码；没有则传 `""`。例如切换自定义 system prompt 或把菜单标题
改成战斗提示，应改变 context。并发数或重试次数不应放入。

<!-- att-example: valid -->
```lua
local prepared = ctx.translation.prepare(
  "database_entry",
  "星港へ",
  "protocol=quest-title;surface=menu"
)
local accepted = prepared:accept("前往星港")
if accepted.accepted then
  ctx.db.begin()
  ctx.db.execute([[
INSERT INTO lua_example_translation
  (identity, original, semantic_context, translation, state)
VALUES (?, ?, ?, ?, ?)
ON CONFLICT(identity) DO UPDATE SET
  original=excluded.original,
  semantic_context=excluded.semantic_context,
  translation=excluded.translation,
  state=excluded.state
]], {
    "quest:arrival:title",
    "星港へ",
    "protocol=quest-title;surface=menu",
    accepted.translation,
    accepted.state,
  })
  ctx.db.commit()
end
```

完整示例在发现旧 pair 不 Current 后，先于外部请求成对删除它；因此 LLM 或验收随后失败，
WriteBack 也不会消费陈旧译文。生产脚本还应像完整示例一样在异常路径 rollback。非法 state（大写、长度错误、非 hex）会
抛 `translation/invalid_state`，不是“旧了所以 false”。不要自行拼 state，也不要把
translation 和 state 分两次提交。

测试预期：第一次无行时请求一次假 LLM；第二次相同输入零请求；改变 original、context、
实际术语、实际 Placeholder、engine、语言/Prompt/Client 语义或最终译文后不再 Current。

`prepared.status` 为 `non_source_language` 或 `fully_protected` 时，官方示例会成对清除旧
translation/state，并保持二者为 NULL：这两种状态不需要也不能伪造一个已验收译文。
后续每次 Translate 仍会重新 `prepare`，但不会请求 LLM。若脚本希望记录“已观察过”，应
在另一张私有表保存独立诊断事实，不能制造虚假的 translation/state pair。

## 3. 已有人工作品交给 Standard 验收

适用条件：目标已经是 Extract 建立的 Standard 物理单元，候选由人完成，需要沿用普通
Standard 的 Placeholder、line shape、语言验收、去重传播和 Current state。不要为这种
情况使用 Translate Lua 标量 `prepare/accept`，也不要直接 SQL 修改受管表。

完整脚本：[lua-accept-standard.lua](examples/lua-accept-standard.lua)。它每次由独立项目
Lua 命令显式读取，不进入任何阶段脚本快照：

<!-- att-example: illustrative -->
```text
att --config att.toml mv lua --name my-game \
  --profile default docs/rpg-maker/examples/lua-accept-standard.lua
```

脚本通过 `standard:units()` 和完整只读身份定位唯一目标，并在提交前复核原文、形状和
状态。复制到真实项目时必须把这些断言与候选同时替换，不能只按遍历序号取“第一个缺失
单元”：

<!-- att-example: valid -->
```lua
local standard = ctx.standard.open()
local target = nil
for unit in standard:units() do
  if unit.owner == "builtin"
     and unit.group_kind == "database_entry"
     and unit.role.kind == "scalar"
     and unit.role.field == "description"
     and unit.original == "药水" then
    assert(target == nil, "目标身份不唯一")
    target = unit
  end
end
assert(target ~= nil, "没有找到待补译单元")

local results = standard:accept({
  {
    unit = target,
    candidate = "人工译文",
    replace_current = false,
  },
})
assert(results[1].accepted, results[1].reason)
```

候选必须使用 `target.model_text` 中的 ATT token，不能照抄原始控制符。Value 和 Lines 也
不能互换：DialogueBody 的 reflow 候选仍是字符串数组；Choices 和 ScrollingText 必须
保持槽数与空槽；严格单行 Value 拒绝 LF。

一次 batch 中，普通拒绝项保持零写入，全部合法去重族在同一事务提交。若候选会改变族中
任一 Current 译文，先人工确认影响，再显式改为 `replace_current=true`；`family_size`
可以提示传播范围，但不能替代对实际单元的审核。成功返回后该次提交已经生效，脚本后续
失败不会撤销它。

省略 `--profile` 时，只有 `ctx.standard.open()` 会尝试复用上次成功 Translate 的 Profile；
普通项目 Lua 可在没有 Profile 时运行。相同 Profile 的下一次 Translate 会跳过仍 Current
的人工族，WriteBack 直接消费它；改变 Prompt、Client、语言、术语、Placeholder、原文或
源上下文后仍会按普通规则失效。

## 4. 幂等 WriteBack

完整脚本：[lua-idempotent-write-back.lua](examples/lua-idempotent-write-back.lua)。WriteBack
每次从冻结 source 建新候选，脚本从候选原文和私有表重建结果，不读取旧 `write_back`，
不维护“已经写过”计数，也不等待不存在的 post-publish hook。

<!-- att-example: valid -->
```lua
assert(ctx.phase == "write_back" and ctx.write_back ~= nil)
local path = "data/QuestEntries.json"
local entries = ctx.output.read_json(path)
local row = ctx.db.query([[
SELECT original, translation
FROM lua_example_translation
WHERE identity = ?
]], { "quest:arrival:title" })
if #row == 1 then
  assert(entries[1].title == row[1][1], "候选原文漂移")
  entries[1].title = row[1][2]
end
ctx.output.write_json(path, entries)
```

这里先断言候选原文，防止把与当前候选结构不一致的私有译文写入新游戏。相同 source 与
私有表重复运行两次，编码结果相同。脚本返回后由 Host 统一验证和发布；不要在返回前把私有表
写成“published”，因为之后仍可能在候选验证、目录发布或必要收尾时失败。

需要布局时，在写 JSON 前调用 `ctx.write_back.layout`，并同时处理 `applied` 与正常的
`manual` 结果；不要自行猜测 Standard 的窗口宽度。

## 5. 插件标签的三阶段私有协议

完整脚本：[lua-private-tag.lua](examples/lua-private-tag.lua)。示例把
`Items.json[1].note` 中的 `<Help:炎の剣の説明>` 当作插件自己的 grammar，而不是 Host
位置类型：

<!-- att-example: illustrative -->
```text
Extract
  读取完整 note Value
  -> Lua 解析唯一 <Help:...>
  -> 私有 identity/original/expected_value

Translate
  私有 original
  -> prepare("database_entry", original, 私有 grammar context)
  -> is_current / LLM / accept
  -> 私有事务成对提交 translation/state

WriteBack
  读取候选的完整 note Value
  -> Lua 按同一 grammar 解析并复核 expected_value/original
  -> 用已验收译文重建完整 <Help:...>
  -> 写回完整 note Value
```

脚本自己决定 `>` 是否能够出现在私有标签值中，并在 Extract、Translate、WriteBack 使用
同一 grammar。`translation.prepare/accept` 只负责公共 Placeholder、术语、语言和 state
语义，不替脚本验证 `<Help:...>`。Host 只提供完整 Value 读取、公共翻译能力、SQLite 和
候选写入；不存在标签扫描器、occurrence 或局部写回助手。

如果希望把整个 `<Help:炎の剣の説明>` 作为一个 Standard Unit，也可以用
`document:text({1, "note"})` 交给 `replace_standard`；此时候选会替换完整 Value，裸
尖括号仍是普通内容。若只翻译正文而保留壳，应该由 Extract Rule 显式 `text` 捕获建立
recipe，或像本节一样由 Lua 私有协议拥有。

## 6. 跨文档、多目标三阶段私有协议

完整脚本：[lua-complex-protocol.lua](examples/lua-complex-protocol.lua)。示例把
`QuestGraph.json[i].title` 与 `QuestIndex.json[id].label` 视为同一个语义事实，并读取
`Actors.json` 和相应 Map 的 `displayName` 形成跨文档 context。

<!-- att-example: illustrative -->
```text
Extract（新 VM/连接）
  QuestGraph + QuestIndex + Actors + Map
       -> 私有 unit(identity, original, semantic_context, translation, state)
       -> 私有 target(identity, target_order, document, path, expected_original)

Translate（新 VM/连接）
  私有 unit -> prepare/current/LLM/accept
             -> 同事务成对更新 translation/state

WriteBack（新 VM/连接）
  私有 unit + target + 新候选
       -> 复核两个 expected_original
       -> 同一译文写入两个文档
       -> 幂等返回，由 Host 验证和发布
```

这个协议没有调用 `replace_standard`，因为一个译文拥有两个目标且 context 跨文档；身份、
继承、目标顺序、事务和漂移检查都由脚本私有表明确拥有。Extract 使用 `seen` 做本次快照
收敛：原文/context 未变时保留 translation/state，任一变化时成对清空，最后删除未见单元。
Translate 也会在外部请求前清除不再 Current 的 pair，避免失败后 WriteBack 使用旧语义。

私有协议只有在实际执行 Translate 脚本时才会调用 `is_current`。术语、Placeholder、公共
Prompt、Client、语言模块、engine、original 或脚本 context 发生变化后，必须在 WriteBack
前重新运行同一份 Lua Translate；跳过 Translate 而直接 WriteBack，核心不会替脚本检查其
私有表，脚本就可能自行消费陈旧 pair。

复制到真实插件时至少替换并验证：

1. 私有 identity 的稳定来源，不使用可变译文或显示顺序充当主键；
2. 所有影响译文的跨文档 context；
3. 每个目标的 expected original、类型、顺序和一对多关系；
4. Extract 失败时旧私有快照是否保持，Translate 每单元或每批事务边界；
5. WriteBack 重复执行是否确定，部分目标失败时是否在写文件前停止；
6. 发布后没有回调时，协议是否仍能从权威输入恢复。

## 7. SQLite 与阶段交接检查

需要私有状态的阶段示例只使用 `lua_example_*` 或 `lua_complex_*` 表；人工 Standard
示例只调用 `ctx.standard`。可信脚本虽然能执行任意单条 SQLite statement，但直接修改
ATT 受管表意味着自行承担不公开稳定的 schema 和全部不变量，也无法正确构造 Standard
state，不应从示例复制。

三个阶段分别有新 VM 和新连接。以下做法无效：

<!-- att-example: invalid -->
```lua
-- Extract 中建立 TEMP 表，期待 Translate 读取：下一阶段连接看不到它。
ctx.db.execute("CREATE TEMP TABLE handoff(value TEXT)")
```

跨阶段只使用持久私有表或 ATT 已明确拥有的标准资产。每个阶段正常返回前必须 commit 或
rollback；活动事务不会被隐式提交。

## 8. 交付前盲测

让未参与脚本编写的人只阅读 [Lua 技术参考](lua.md)、本页和目标游戏协议材料，然后用
隔离夹具验证：

- Extract 首次建立、重复收敛、删除来源、原文/context 改变；
- Translate 首跑、二跑零 LLM、有效语义变化后重译、accept 拒绝路径；
- 独立 Lua 的 Value/Lines 形状、同族冲突、Current 覆盖、原子回滚和相同 Profile 零 LLM；
- WriteBack 两次字节结果一致、任一目标漂移时不留下半修改；
- MV/MZ 都只使用 `data/js` 逻辑路径；
- 数据库事务关闭，TEMP/globals 未被误作阶段交接；
- 候选发布失败时脚本不声称已发布。
