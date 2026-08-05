# 原子数据库 Lua 现行规格

Lua 是 MV、MZ 和 Generic 项目的独立数据库操作命令：

```text
att mv lua --name NAME SCRIPT.lua [-- ARG...]
att mz lua --name NAME SCRIPT.lua [-- ARG...]
att generic lua --name NAME SCRIPT.lua [-- ARG...]
```

它用于完整审查当前 Unit、人工或 agent 精确修订译文、批量维护项目数据库，或维护脚本
自己的私有表。ATT 提供受控数据库连接，整个脚本只有一次提交或一次回滚。

Lua 不属于 Init、Extract、Translate 或 WriteBack。它不保存脚本、不请求模型，也不读取
游戏、外部 JSONL 或输出文件。范围调查、失败选择和最终验收分别见
[翻译项目指南](../guides/translation-project.md)、
[诊断与恢复指南](../guides/diagnosis-and-recovery.md)和
[全量验收指南](../guides/acceptance.md)。

## 1. 程序与环境

ATT 每次从显式路径读取并编译一个 UTF-8 Lua 5.4 主程序。全局 `arg[0]` 是脚本路径，
`arg[1]` 起是 `--` 后按原顺序提供的 UTF-8 字符串；没有参数时只有 `arg[0]`。

VM 只提供 base、coroutine、table、string、math 和 utf8：

- 不提供 `io`、`os`、`package`、`require`、`loadfile`、`dofile`、`debug` 或 `warn`；
- 不提供文件、网络、进程、环境变量、LLM、来源或输出 API；
- 每次 `print(...)` 产生一条经过安全处理的 `lua.print` 项目日志，不直接写 stdout；
- 脚本大小、查询行数、返回字节、运行时间和内存不设产品上限。

每次运行建立全新 VM；运行结束后，只有提交到项目数据库的内容保留。

## 2. `ctx` 与 SQL

```lua
ctx.project.name
ctx.project.engine

ctx.db.NULL
ctx.db.blob(bytes)
ctx.db.query(sql, parameters)
ctx.db.execute(sql, parameters)

ctx.translation.set(locator, translation)
ctx.translation.clear(locator)
```

`ctx.project` 只读，`engine` 是 `mv`、`mz` 或 `generic`。

### 2.1 SQLite 值

| SQLite | Lua |
| --- | --- |
| NULL | `ctx.db.NULL` |
| INTEGER | Lua integer |
| REAL | 有限 Lua number |
| TEXT | UTF-8 Lua string |
| BLOB | `ctx.db.blob` 值 |

`ctx.db.blob(bytes)` 从 Lua string 建立不可伪造的 BLOB 值。query 返回的 BLOB 使用同一
类型，以 `value:bytes()` 取得逐字 Lua string。Lua `nil` 不是 SQL NULL；SQL NULL 一律用
`ctx.db.NULL` 表示。

### 2.2 query 与 execute

```lua
local rows = ctx.db.query(
  "SELECT group_id, unit_id FROM generic_unit WHERE group_id = ?1",
  { "opening" }
)

local changed = ctx.db.execute(
  "UPDATE lua_notes SET note = ?1 WHERE key = ?2",
  { "checked", "menu" }
)
```

- 每次调用只接受一条完整 SQLite statement；
- 参数是可省略的从 1 开始、无洞数组；
- `query` 只接受 SQLite 判定为只读且会返回列的 statement，返回二维稠密数组，不附列名；
- `execute` 返回 direct changed rows，不包含 trigger 或 foreign-key action 的间接修改；
- SQL 或参数错误可以由 `pcall` 捕获，捕获后脚本可以继续；
- SQL、参数、查询结果和游戏正文不会自动进入普通项目日志。

普通查询、DML，以及为脚本私有数据建立或修改事务性表和索引都允许。私有对象建议使用
`lua_` 前缀。读取或直接修改 ATT 表属于可信高级操作，当前相关表与列见
[SQLite 规格](../runtime/sqlite.md#4-lua-审查使用的当前表)。

MV/MZ 的 `group_location` 只保存在 Group 表。Unit 和 Mutation Claim 使用 owner 内的
`group_id` 关联 Group；审查 Unit 必须显式 JOIN 当前表，不能从 Unit 读取旧列：

```lua
local rows = ctx.db.query([=[
  SELECT text_group.group_location, text_unit.unit_role
  FROM rpg_maker_text_unit AS text_unit
  JOIN rpg_maker_text_group AS text_group
    ON text_group.owner = text_unit.owner
   AND text_group.group_id = text_unit.group_id
  WHERE text_unit.owner = ?1
  ORDER BY text_group.semantic_order_key, text_unit.semantic_order_key
]=], { "builtin" })
```

这是当前 raw schema；没有兼容 view、旧列别名或历史格式读取。直接修改 Unit 时，先用同一
`owner + group_id` 关系定位行，或在 `WHERE` 中从 Group 按 `owner + group_location` 取得
`group_id`。

以下操作始终拒绝：

- `BEGIN`、`COMMIT`、`ROLLBACK`、`SAVEPOINT` 和 `RELEASE`；
- `ATTACH`、`DETACH` 和扩展装载；
- 删除、改名或改变 ATT 自有表、索引、触发器和 schema；
- 一次调用中包含第二条 statement。

## 3. 精确译文操作

`ctx.translation` 精确处理 locator 指定的单个 Unit，不触发全局去重或传播。

Generic：

```lua
ctx.translation.set(
  { group_id = "opening", unit_id = "body" },
  "你好。\n今天天气真不错。"
)
```

MV/MZ 的 locator 必须使用审查查询返回的原值：

```lua
ctx.translation.set(
  {
    owner = "builtin",
    group_location = retrieved_group_location,
    unit_role = retrieved_unit_role,
  },
  { "第一行", "第二行" }
)
```

- Generic 译文必须是非空白字符串，允许 LF，不允许 CR 或 NUL；
- MV/MZ 根据 Unit 源形状接受字符串或稠密字符串数组，并检查形状、空槽、控制符与
  Placeholder；
- Generic locator 是 `group_id + unit_id`；MV/MZ locator 是
  `owner + group_location + unit_role`；
- MV/MZ 表内的整数 `group_id` 只负责 owner 内关联，不属于 locator；
- MV/MZ 的 `group_location` 与 `unit_role` 是不透明稳定字符串，必须逐字使用数据库原值；
- `set` 写入人工翻译状态，人工与 agent 修订使用同一状态；
- `clear(locator)` 同时清除该 Unit 的译文与状态；
- 来源、Group 语境、语言、结构或实际 Placeholder 改变时，人工状态失效；
- 术语、Prompt、Profile 或 Client 改变不影响人工状态。

用直接 SQL 修改一个已有 Current 的目标译文时，原语义状态保持。要修改未译 Unit、建立
人工状态或执行形状与 Placeholder 检查时，必须使用 `ctx.translation.set`，不能用 SQL
伪造 `translation_state`。

## 4. 完整审查与人工或 agent 修订

### 4.1 导出当前全部 Unit

使用[完整 Unit 审查脚本](examples/export-unit-review.lua)：

```text
att mv|mz|generic lua --name NAME docs/lua/examples/export-unit-review.lua
```

路径仍按 CLI 规格从调用 cwd 解析；实际执行时应使用脚本的绝对路径或从正确 cwd 调用。
脚本只查询，但 Lua 命令本身仍取得项目排他租约、执行 `BEGIN IMMEDIATE`、最终验证并提交，
因此不属于纯只读 CLI。完成或修复翻译的任务已包含这项项目内操作；只读调查必须有写入
授权才能执行。

脚本按项目自然顺序把全部 Unit 聚合成一份 UTF-8 `att-unit-review-v1` JSON，再把每个字节
编码成 lowercase hex，只调用一次 `print`。结果位于
[本次项目日志](../runtime/project-log.md)唯一 `lua.print` 事件的 `payload.message` 中，格式是：

```text
att-unit-review-v1-hex:<十进制 UTF-8 字节数>:<lowercase hex>
```

这不是临时 stdout：完整源文、已有译文、上下文和 locator 会以可逆 hex 持久写入
`<project>/logs/<run-id>.jsonl`。hex 只避免日志单行净化破坏字节，不是脱敏或加密。执行前
必须确认任务允许把全部项目文本写入该日志，并按完整游戏文本处理其访问、保留和清理；
只读调查没有这项授权时不得运行。完成或修复翻译的任务已包含这项项目内写入。

必须从结构化 JSONL 事件读取 `payload.message`；顶层本地化 `message` 带有展示前缀，不能
用来解码。把 `payload.message` 按两个冒号拆分，核对固定前缀、十进制字节数、
hex 只含 `0-9a-f` 且长度恰好为字节数的两倍，再逐对解码成 UTF-8 JSON。不要把终端展示
文本或未解码的 hex 当作游戏正文。

解码后的 JSON：

- Generic 包含 kind、`group_id`、`unit_id`、源文、当前译文和 automatic/manual origin；
- MV/MZ 包含 kind、owner、`group_location`、`unit_role`，以及三个保存内层 JSON 文本的
  字符串字段。外层 JSON 解码后，必须再分别解析 `source_content_json`、
  `source_context_json` 和非 null 的 `translation_content_json`；它们不是已经嵌入外层的
  JSON 值。`source_content_json` 解析为 string 或 string array，`source_context_json`
  解析为 object，`translation_content_json` 解析为 null、string 或 string array。不得把
  这些字段的引号和反斜杠当作游戏正文翻译；
- `unit_count` 必须等于 `units` 数组长度。

导出只有在以下事实全部成立时才完整：

- 日志顶层 `context.engine`、`context.project`、`context.command = "lua"` 与本次项目一致，
  全部检查使用同一 RunId；
- 恰好一条 `lua.script` 的 `payload.identity` 是实际解析后的脚本路径，
  `payload.fingerprint` 等于这份脚本 UTF-8 原始字节的 SHA-256；
- 恰好一条 `lua.summary`，其 `payload` 为 `database_calls = 1`、`changed_rows = 0`、
  `translation_calls = 0`、`printed_lines = 1`；
- 恰好一条 `lua.print`，没有日志丢弃、写入降级或呈现失败诊断，唯一 `run.finished` 的
  `payload.result.kind = "succeeded"`；
- 信封前缀、字节数和 hex 完整性有效；解码结果是有效 UTF-8 与 JSON；
  `format = "att-unit-review-v1"`；`unit_count` 与 `units` 数组长度一致。

十六进制信封只含不会被 `lua.print` 单行净化改写的 ASCII，解码后才是数据库返回的逐字
JSON 字节。脚本在聚合函数内部按项目自然顺序排序；不能删掉该 `ORDER BY`，也不能把子查询
行序当作 SQLite 聚合顺序保证。

### 4.2 不把空译文直接等同于待翻译

Generic 的 `translation = null` 和 MV/MZ 的 `translation_content_json = null` 只表示数据库
没有译文状态。空白、没有源语 NaturalText 或完全受 Placeholder 保护的 Unit 也可能合法
保持 null；真正 `needs_translation` 是 Translate 根据当前语言和 Placeholder 当场计算的
运行时事实，不单独持久化。

因此，人工或 agent 必须结合完整 Group、源内容、上下文、语言和 Placeholder，把每个 null
Unit 分类为：

- 应翻译且尚未有译文；
- 按规格不适用并应保留原文；
- 上游提取、分组、语言或 Placeholder 有误，应返回相应阶段；
- 当前证据不足，不能擅自写入。

### 4.3 从任务失败回到稳定 locator

模型任务记录中的数字 ID 只在一次请求内有效，不能直接充当数据库 locator。先从该次
`task.finished.payload.outcome.diagnostic` 取得 occurrence ID，再在同一 RunId 的项目日志中
读取对应 `diagnostic.translation_task` occurrence：

- issue 的 task-response `scope.kind = "unit"` 时，`scope.unit` 已保存 MV/MZ 的 `owner`、
  `group_location` 与 `role`。这三项就是失败 Unit 的稳定 locator 原值；仍应结合任务记录和
  完整 Group 语境确认要写入的译文，但不需要按重复原文猜位置；
- `scope.kind = "task"` 时，失败发生在整个响应、HTTP 或任务边界，本来就没有唯一失败
  Unit。此时结合任务记录中的本次输入、当前数据库审查和仍未 Current 的 Unit 逐项判断，
  不能把任务序号、临时 ID 或任意一段原文伪装成 locator。

诊断 occurrence 与任务记录都是定位证据，不是数据库权威。最终写入仍使用审查导出的精确
locator；Generic 继续使用 `group_id + unit_id`，MV/MZ 继续使用
`owner + group_location + unit_role`。

### 4.4 生成并执行修订脚本

把已经确认的编辑写进任务材料中的独立 Lua 脚本。对每个 Unit 调用一次
`ctx.translation.set`，保留精确 locator 和正确的 string 或 string-array 形状。执行前记录
脚本、参数和 SHA-256；执行后确认事务提交，再重新运行审查导出并核对所有受影响 Group。

agent 可以直接完成翻译和脚本生成，无需把剩余内容继续交给模型 Client；但它必须遵守
当前语言、术语、Placeholder、人物语气和完整语境要求。`set` 只负责结构、Placeholder 与
数据库状态，不检查术语、文风、目标语言质量或源语残留。

### 4.5 能力边界

Lua 没有模型、语言分析、Placeholder 投影、文件输出或实际游戏加载 API。当前 CLI 也没有
独立的 `status`、`inspect`、`list-untranslated` 或失败 ID 映射命令。审查脚本提供当前
数据库的完整 Unit、译文和 locator，但“是否应该翻译”仍需人工或 agent 按规格判断。

导出按项目自然顺序列出所有 Group，却不公开解码 MV/MZ Semantic Scope、Generic UTF-16
来源路径或重建某次 Translate 的 TaskBlock 边界。需要模型请求的原始完整 TaskBlock 时，
仍要读取对应任务记录；任务记录与导出之间没有通用的稳定 ID 连接，歧义内容只能依靠
完整来源、相邻语境和实际消费者逐项确认。

## 5. 单一事务

一次运行严格按照以下顺序：

```text
读取并编译脚本
→ 取得项目排他租约
→ 打开已有项目数据库
→ BEGIN IMMEDIATE
→ 执行整个 Lua
→ 验证 ATT schema、metadata、领域不变量、foreign_key_check、quick_check
→ COMMIT
```

- 语法错误发生在事务开始前；
- 未捕获的 Lua、Host 或 SQL 错误，取消、panic 或最终验证失败都会回滚；
- 脚本不能提前提交，也不能把一次调用拆成多个事务；
- `pcall` 捕获普通错误后可以继续，但最终数据库验证始终执行；
- VM instruction hook、SQLite progress 与 busy 机制共同响应取消；
- COMMIT 开始后 ATT 等待 SQLite 的实际结果；无法确认时报告 `outcome_unknown`。

MV/MZ 的事务基线和最终验证只要求实际存在译文或译文状态的 Unit 继续满足当前语义。一个
Unit 的 `translation_content_json` 与 `translation_state` 都为 null 时，它自己的
Placeholder 等局部规划错误不会阻断无关 Lua、`clear` 或其他精确修订；`ctx.translation.set`
仍会当场校验目标 Unit，脚本前后继续存活的 manual/automatic 译文也必须保持形状、
Placeholder、Group 语境和相应状态有效。失败诊断保存精确 `mv`/`mz` 引擎、Unit locator 与
具体 Placeholder 问题，不再退化为 `engine = generic`、无 locator 的普通
`state_mismatch`。

Generic 仍遵守其 Translate 的全项目规划契约：任一 Unit 的 Placeholder 等规划错误会在
所有 Task 前使 Translate 失败，因此 Generic Lua 的事务前置和最终验证也保持同一要求，
不能用 MV/MZ 的 null Unit 规则绕过。

项目租约覆盖打开数据库、事务、验证、提交或回滚及最终结果。普通日志失败不改变数据库
结果，但会使依赖日志输出的审查导出缺少完整证据。

## 6. 日志与恢复

运行日志记录命令、脚本路径与 SHA-256、引擎、项目、数据库调用次数、changed rows 和最终
事务状态。项目日志建立后的每次运行只写一条 `lua.summary`；操作统计不表示已经提交，
事务终态只由 `run.finished` 表达。每次显式 `print` 另写一条 `lua.print`。

成功后重新读取需要确认的项目状态。失败且明确回滚时，可以修正脚本后重做；
`outcome_unknown` 时停止同一项目的写入和重跑，保留现场并按
[诊断与恢复指南](../guides/diagnosis-and-recovery.md#45-outcome_unknown)处理。

可复制示例：

- [导出完整 Unit 审查十六进制信封](examples/export-unit-review.lua)
- [精确修订 Generic 译文](examples/generic-override.lua)
- [维护脚本私有表](examples/project-note.lua)
- [失败时整体回滚](examples/rollback.lua)
