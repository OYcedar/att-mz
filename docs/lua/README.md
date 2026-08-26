# 项目数据库 Lua 现行规格

Lua 是 MV、MZ 和 Generic 的数据库脚本入口：

```text
att mv lua --name NAME SCRIPT.lua [-- ARG...]
att mz lua --name NAME SCRIPT.lua [-- ARG...]
att generic lua --name NAME SCRIPT.lua [-- ARG...]
```

普通人工补译优先使用 [Manual TOML](../manual/README.md)。Lua 适合批量读取上下文、复杂筛选、
计算生成、批量变换、诊断和特殊数据库修改。高级 API 使用可读 ID；原始数据库 API 则故意
允许绕过 ATT 的翻译规则和状态保护。

## 1. 程序与环境

ATT 每次从显式路径读取并编译一个 UTF-8 Lua 5.4 主程序。全局 `arg[0]` 是脚本路径，
`arg[1]` 起是 `--` 后按原顺序提供的 UTF-8 字符串。

VM 提供 base、coroutine、table、string、math 和 utf8，不提供 `io`、`os`、`package`、
`require`、`loadfile`、`dofile`、`debug`、文件、网络、进程、环境变量或模型 API。
`print(...)` 写入一条经过安全处理的 `lua.print` 项目日志，不直接写 stdout。

脚本运行在项目租约内。MV/MZ 打开当前项目数据库；Generic 直接打开工作区中的
`project.db`，即使 ATT 表已经被 raw SQL 删除，仍可再次运行 Lua 进行调查或继续修改。

## 2. 高级翻译 API

```lua
ctx.project.name
ctx.project.engine

ctx.translation.list()
ctx.translation.list({ status = "unfinished", ids = { "Skills.json:798:name" } })
ctx.translation.context({ "id1", "id2" })
ctx.translation.set("Skills.json:798:name", { "尾击" })
ctx.translation.clear("Skills.json:798:name")
ctx.terminology.list()
```

`ctx.project` 只读，`engine` 是 `mv`、`mz` 或 `generic`。

### 2.1 list

`ctx.translation.list()` 按项目自然顺序返回全部当前条目，并在末尾包含已经失去当前位置的
过期人工记录。可选筛选表只允许：

- `status`：`unfinished`、`translated`、`not_needed` 或 `outdated`；
- `ids`：可读 ID 的无重复字符串数组。

每项包含 `id`、`type`、`source` 和 `status`；存在当前译文时还包含
`translation` 和 `origin`，其中 origin 是 `manual` 或 `automatic`。存在过期人工
译文时，`outdated_manual` 保存旧 `id`、`type`、`source` 和 `translation`。
所有正文都是字符串数组。

状态优先表达当前可消费事实：存在当前人工或自动译文时为 `translated`；没有当前译文但
需要翻译或存在 Rejected 候选时为 `unfinished`；只有前两者都不成立且仍保留过期人工快照时
才为 `outdated`；其余为 `not_needed`。Rejected 不得被误报为 `not_needed`，过期人工快照也
不得遮蔽同一位置的当前译文。

### 2.2 context

`ctx.translation.context(ids)` 一次接收多个可读 ID，并按请求顺序返回每个 ID 所属逻辑
Group：

```lua
local groups = ctx.translation.context({
  "Map023.json:event17:page1:dialogue42",
  "Map023.json:event17:page1:dialogue43",
})

for _, group in ipairs(groups) do
  print(group.id, group.speaker or "")
  for _, item in ipairs(group.translations) do
    print(item.id, table.concat(item.source, " / "))
  end
end
```

每个结果包含请求的 `id`、可用时的 `speaker`，以及该逻辑 Group 的完整
`translations`。不要为每条译文分别启动一次 Lua；先合并全部待查 ID。

### 2.3 set 与 clear

`ctx.translation.set(id, translation)` 只接受可读 ID 和非空字符串数组，复用 Manual 的
type、数组形状、固定空槽、控制码和 Placeholder 检查。写入后人工译文优先于自动译文。
人工记录绑定写入时的项目语言对，不绑定兄弟 Unit 或完整 Group 语境。

`ctx.translation.clear(id)` 删除该位置的人工记录和自动译文，使当前条目回到未完成或
不需要翻译的实际状态；它也可以清除只剩过期快照的位置。

这两个高级操作不会调用模型、修改全局规则或使无关译文失效。输入只接受当前可读 ID 和
字符串数组译文。

### 2.4 术语

`ctx.terminology.list()` 返回按当前定义顺序排列的 `{ term, translation }`。补译时应主动
读取并参考它；`set` 本身不判断术语、文风、残留源语或译文质量。

## 3. 原始数据库 API

```lua
ctx.db.NULL
ctx.db.blob(bytes)
ctx.db.query(sql, parameters)
ctx.db.execute(sql, parameters)
```

SQLite 值映射如下：

| SQLite | Lua |
| --- | --- |
| NULL | `ctx.db.NULL` |
| INTEGER | Lua integer |
| REAL | 有限 Lua number |
| TEXT | UTF-8 Lua string |
| BLOB | `ctx.db.blob` 值；用 `:bytes()` 取回字节 |

每次调用只接受一条完整 statement，参数是可省略的从 1 开始无洞数组。
`query` 接受任意有返回列的 statement，返回二维稠密数组；`execute` 接受任意无返回列的
statement，并返回 direct changed rows。DML、DDL、PRAGMA 和显式事务均允许。

raw SQL 不做表白名单、schema 版本检查、强制备份、强制事务、领域校验、自动修复、
`foreign_key_check` 或 `quick_check`。它可以写乱码状态、制造孤儿关系、关闭外键、删除
数据或表，并使整个项目无法再被 ATT 普通命令打开。成功只表示脚本执行结束，不表示数据库
仍然有效。

以下操作仍被拒绝，避免访问其他数据库文件或加载本机扩展：

- `ATTACH`
- `DETACH`
- `load_extension`
- 一次调用中的第二条 statement

## 4. 事务与失败

连接从 autocommit 开始，脚本自行决定是否使用事务：

```lua
ctx.db.execute("BEGIN IMMEDIATE")
ctx.db.execute("UPDATE ...")
ctx.db.execute("COMMIT")
```

- autocommit 修改在 statement 成功后立即保留；
- 显式 `COMMIT` 的修改不会因脚本稍后失败而撤销；
- 失败、取消或 panic 只回滚当时仍打开的事务；
- 正常结束时仍有事务未关闭，ATT 报错并回滚该事务；
- `pcall` 可以捕获 SQL 或高级 API 错误并继续。

每次高级 `set` 或 `clear` 在自己的 savepoint 中完整成败。`RELEASE` 失败时，ATT 在事务仍
活动时先回滚该高级操作；`RELEASE`、`ROLLBACK TO` 或 savepoint 清理失败都会立即毒化本次
脚本执行，不能被 `pcall` 吞掉后继续提交外层事务。运行根随后回滚仍活动的根事务；根回滚
失败按结果未知报告。最外层 `RELEASE` 报错且 SQLite 已经结束事务时，ATT 无法从返回码判断
提交还是自动回滚，直接按结果未知报告，不伪称已经回滚。这不改变 raw SQL 和脚本显式事务
在没有高级 API 清理失败时的责任边界。

ATT 不在脚本结束时检查或修复业务状态。需要一组修改共同成败时，由脚本显式开始、提交或
回滚事务；不需要时可以直接使用 autocommit。

## 5. 日志与使用边界

项目日志记录 Lua 命令的普通运行事件和显式 `lua.print`，不保存脚本摘要或无实际作用的
调用汇总。SQL、参数、查询结果和游戏正文不会自动进入日志。

高级 API 依赖当前 ATT schema；数据库已被 raw SQL 破坏时，它可以失败。raw API 不执行
ATT schema 校验，因此仍可用于调查或继续破坏当前 `project.db`。

保留示例：

- [用可读 ID 修改 Generic 译文](examples/generic-override.lua)
- [在显式事务中维护脚本私有表](examples/project-note.lua)
- [未捕获错误只回滚仍打开的事务](examples/rollback.lua)
