# 原子数据库 Lua 现行规格

Lua 是 MV、MZ 和 Generic 项目的独立数据库操作命令：

```text
att mv lua --name NAME SCRIPT.lua [-- ARG...]
att mz lua --name NAME SCRIPT.lua [-- ARG...]
att generic lua --name NAME SCRIPT.lua [-- ARG...]
```

它适合精确修订某个译文、批量更新项目数据库或维护脚本自己的私有表，作用类似 Redis
中的 Lua：程序提供受控数据库连接，整个脚本只有一次提交或一次回滚。

Lua 只面向项目数据库，与 Init、Extract、Translate、WriteBack 互不干涉。ATT 不保存
脚本，脚本也不请求模型；游戏、JSONL 输入、候选和输出文件都在它的读写范围之外。

## 1. 程序与环境

ATT 每次从显式路径读取并编译一个 UTF-8 Lua 5.4 主程序。全局 `arg[0]` 是脚本路径，
`arg[1]` 起是 `--` 后按原顺序提供的 UTF-8 字符串；没有参数时只有 `arg[0]`。

VM 只提供 base、coroutine、table、string、math 和 utf8：

- 不提供 `io`、`os`、`package`、`require`、`loadfile`、`dofile`、`debug` 或 `warn`；
- 不提供文件、网络、进程、环境变量、LLM、来源或输出 API；
- 每次 `print(...)` 都会产生一条经过安全处理的 `lua.print` 项目日志，不直接写 C stdout；
- 脚本大小、查询行数、返回字节、运行时间和内存都不设产品上限。

每次运行都会创建全新的 VM；运行结束后，只有提交到项目数据库的内容会保留，全局变量、
闭包和 userdata 都随之消失。

## 2. `ctx`

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

`ctx.project` 只读。`engine` 是 `mv`、`mz` 或 `generic`。

### 2.1 SQLite 值

| SQLite | Lua |
|---|---|
| NULL | `ctx.db.NULL` |
| INTEGER | Lua integer |
| REAL | 有限 Lua number |
| TEXT | UTF-8 Lua string |
| BLOB | `ctx.db.blob` 值 |

`ctx.db.blob(bytes)` 从 Lua string 建立不可伪造的 BLOB 值。query 返回的 BLOB 使用同一
类型，以 `value:bytes()` 取得逐字 Lua string。Lua `nil` 不是 SQL NULL，稠密参数与结果
中也不会出现；SQL NULL 一律用 `ctx.db.NULL` 表示。

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
- `query` 只接受 SQLite 判定为只读且会返回列的 statement，返回二维稠密数组，行和列都按
  SQLite 原顺序，不附加列名；
- `execute` 返回 SQLite 的 direct changed rows，不包含 trigger 或 foreign-key action
  间接修改的行；
- SQL 或参数错误可以由 `pcall` 捕获，捕获后脚本可以继续；
- ATT 不自动把 SQL、参数、查询结果或游戏正文写入普通项目日志；脚本显式传给 `print(...)`
  的内容按 `lua.print` 规则处理。

普通查询、DML，以及为脚本私有数据建立或修改事务性表和索引都在允许范围内。建议私有
对象使用 `lua_` 前缀。脚本也可以直接修改 ATT 表，但这是可信的高级操作，动手前请先
理解当前数据库规格。

以下操作始终拒绝：

- `BEGIN`、`COMMIT`、`ROLLBACK`、`SAVEPOINT` 和 `RELEASE`；
- `ATTACH`、`DETACH` 和扩展装载；
- 删除、改名或改变 ATT 自有表、索引、触发器和 schema；
- 一次调用中包含第二条 statement。

这些限制由 SQLite authorizer 和已准备 statement 判断，不靠搜索 SQL 文本。

## 3. 精确译文操作

`ctx.translation` 是修改常见译文的安全接口，精确处理 locator 指定的单个 Unit，不触发
全局去重或传播。

Generic：

```lua
ctx.translation.set(
  { group_id = "opening", unit_id = "body" },
  "你好。\n今天天气真不错。"
)
```

MV/MZ：

```lua
ctx.translation.set(
  {
    owner = "builtin",
    group_location = "从项目数据库读取的精确身份",
    unit_role = "从项目数据库读取的精确身份"
  },
  { "第一行", "第二行" }
)
```

- Generic 译文必须是非空白字符串，允许 LF，不允许 CR 或 NUL；
- MV/MZ 根据 Unit 形状接受字符串或稠密字符串数组，并执行形状、空槽、控制符和
  Placeholder 检查；
- locator 必须精确命中当前 Unit，MV/MZ 的 `group_location` 与 `unit_role` 是数据库中的
  不透明稳定字符串，请直接使用数据库原值，不要自行拼接；
- `set` 写入人工翻译状态；
- `clear(locator)` 同时清除该 Unit 的译文与状态；
- 源文、Group 语境、语言或实际 Placeholder 改变时，人工状态失效；
- 术语、Prompt、Profile 或 Client 改变不影响人工状态。

用直接 SQL 修改一个已有 Current 的目标译文时，原语义状态保持，下一次 Translate 仍把
它视为 Current。修改未译 Unit、需要形状检查或需要建立人工状态时，使用
`ctx.translation.set`。

## 4. 单一事务

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
- COMMIT 开始后 ATT 等待 SQLite 的实际结果。无法确认时报告 `outcome_unknown`，而不是
  声称已回滚。

项目租约从脚本编译完成后开始，覆盖打开项目数据库、事务、验证、提交或回滚以及最终
结果。普通日志失败不影响数据库结果。

## 5. 日志与恢复

运行日志记录命令、脚本路径与普通文件 SHA-256、引擎、项目、数据库调用次数、changed
rows 和最终事务状态。项目日志建立后的成功、失败、取消及执行前失败都只写一条
`lua.summary`；其中的调用与改动行数是实际发生过的操作统计，不表示事务已经提交，最终
状态以 `run.finished` 为准。
每次显式 `print(...)` 另写一条 `lua.print` 事件；事件正文会移除控制字符伪装并采用项目
日志统一的安全处理。

日志只自动记录上述运行元信息。SQL、参数、查询结果、Lua 变量和游戏正文是否进入日志，
完全由脚本决定：只有脚本显式 `print(...)` 的内容才会作为 `lua.print` 的正文写入项目
日志。因此脚本只应打印操作者确实需要保留的诊断。

成功后重新读取需要确认的项目状态。失败且明确回滚时，可以修正脚本后重试；
`outcome_unknown` 时停止写入和重跑，保留现场并按诊断重新观察数据库。

可复制示例：

- [精确修订 Generic 译文](examples/generic-override.lua)
- [维护脚本私有表](examples/project-note.lua)
- [失败时整体回滚](examples/rollback.lua)
