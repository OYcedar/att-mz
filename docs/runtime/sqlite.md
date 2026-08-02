# ATT SQLite 现行规格

每个 MV、MZ 或 Generic 项目都使用自己工作区内的 `project.db`；引擎不同、项目名
不同，表、译文和资源就各归各，互不共享。

## 1. 项目数据库

数据库保存当前产品实际需要的项目事实：

- 项目身份、来源路径或来源指纹、语言；
- Extract 后的 Group、Unit、顺序与写回关系；
- Unit 译文和语义状态；
- 当前术语、Placeholder 与最近 Profile；
- MV/MZ 当前 Builtin/Rules 选择与必要的发布交接。

Generic 的项目库只存自己的工作；外部 JSONL 副本、去重族、代表项、译文历史和
kind 注册表都留在外部。

项目只认严格的当前 schema；不符合当前 schema 的数据库按普通无效项目处理，运行时
不做识别、迁移或兼容。

## 2. 读取与写事务

只读规划基于一致快照；写操作使用短事务、批量 statement 和准备语句，一批工作一起
提交，而不是每个 Unit 各建一个持久事务。

Extract 的同一 owner 或整个 Generic 同步原子提交。Translate 准备阶段原子处理失效
和资源替换，各模型任务按自然顺序独立提交，有效前序进度因此得以保留。WriteBack
只读数据库，输出发布交给目录发布协议。

写事务从开始、COMMIT、ROLLBACK 到无法确认结果，每一步都形成结构化诊断。`busy`
等待期间随时响应取消；项目再忙也只是等待，不会被固定的本地队列容量拒之门外。

## 3. 译文状态

目标译文正文与语义状态分开保存。模型任务提交使用 CAS 同时比较当前源事实、旧目标
正文和旧状态，并发修订不会被悄悄覆盖。

自动状态绑定语言、Prompt、Client、Group 语境、实际术语和 Placeholder。人工 Lua
状态只绑定来源、Group 语境、语言、结构和实际 Placeholder。各引擎规格决定失效
范围。

## 4. Lua 审查使用的当前表

当前发行版没有独立的项目状态或 Unit 导出命令。可信操作者可以通过
[Lua](../lua/README.md)的只读 SQL 查看当前数据库；下面这些表和列是本发行版审查与精确
locator 所需的权威位置。

Generic：

| 表 | 审查使用的列 |
| --- | --- |
| `generic_file` | `relative_path`、`ordinal` |
| `generic_group` | `group_id`、`relative_path`、`ordinal`、`kind` |
| `generic_unit` | `group_id`、`unit_id`、`ordinal`、`source_text`、`translation`、`translation_origin`、`translation_state` |

Generic 自然顺序是 file、group、unit ordinal；精确 locator 是 `group_id + unit_id`。
`translation`、`translation_origin` 和 `translation_state` 要么全空，要么全有。

MV/MZ：

| 表 | 审查使用的列 |
| --- | --- |
| `rpg_maker_text_group` | `owner`、`group_id`、`group_location`、`semantic_order_key`、`group_kind` |
| `rpg_maker_text_unit` | `owner`、`group_id`、`unit_role`、`semantic_order_key`、`source_content_json`、`source_context_json`、`translation_content_json`、`translation_state` |
| `rpg_maker_mutation_claim` | `owner`、`group_id`、`resource_key`、`access` |

`group_id` 是每个 owner 内从 1 开始分配的当前存储关联键，不是公开 locator。Unit 和
Mutation Claim 不重复保存 `group_location`；查询它们时必须用 `owner + group_id` JOIN
`rpg_maker_text_group`，再从 Group 取得 `group_location`。当前 schema 没有为旧列提供 view、
别名或兼容读取。

MV/MZ 按 Group 与 Unit 的 `semantic_order_key` 排列；一个完整逻辑 Group 由 JOIN 后具有相同
`group_location` 的全部 Unit 组成，可以同时包含 builtin 与 rules owner。owner 仍属于精确
locator，后者是 `owner + group_location + unit_role`；`group_id` 不进入 locator。
`group_location` 与 `unit_role` 是不透明编码，只能逐字使用。数据库不单独保存人工/自动 origin；
`translation_state` 也是不透明指纹，不能由 SQL 自行解释。

两种引擎中，译文列为 NULL 只表示没有译文状态，不等于“应当翻译”。空白、没有源语
NaturalText 或完全受保护的 Unit 也可以合法保持 NULL。真正的 `needs_translation`、自动
译文是否适配本次 Prompt/Profile/Client，以及 RPG Maker Semantic Scope 与 TaskBlock，
由 Translate 使用当前资源在运行时计算，不是上述表中的持久字段。

因此 SQL 可以完整列出当前 Unit、译文与 locator，却不能单独代替 Translate 的状态判断，
也不能把任务记录临时 ID 可靠映射成 locator。完整审查方法和导出完整性条件见
[Lua 审查流程](../lua/README.md#4-完整审查与人工或-agent-修订)。

## 5. Lua 会话

独立 Lua 使用同一个项目数据库，外面包着一个 `BEGIN IMMEDIATE`；事务边界由 ATT
掌握，脚本只管写逻辑。运行结束后 ATT 先验证 schema、metadata、领域不变量、
`foreign_key_check` 与 `quick_check`，再提交或回滚。完整 API 与 SQL 限制见
[原子数据库 Lua](../lua/README.md)。

Lua 可以建立自己的私有表，ATT 不读取、迁移或解释它们。直接修改 ATT 表属于可信
高级操作：可以做，但结果必须通过最终不变量检查。

## 6. 诊断与日志

SQLite 出错时，`SqliteIssue.context` 保存 stage、operation 和 transaction；具体 problem
保存数据库路径、query ID/ordinal、driver kind、primary/extended code、column index/name、
SQL offset、参数数量或 changed rows 等当时确实存在的结构化数值。根数据库尚未解析路径时
使用对应的 root problem，不伪造路径。SQL、参数、结果与游戏正文不进普通项目日志，也
不保存 `rusqlite::Error::to_string()` 形成的小协议。

CLI 与 JSONL 消费同一份 `DiagnosticReport`。事务本身的主错误与 rollback、finalization、
shutdown 等相关错误使用明确 relation 保存在同一原子 occurrence 内；report 的 `effect`
说明状态未变、已经提交、收尾失败、需要恢复或结果未知。数据库的恢复和重放另有依据，
项目日志不参与。

事务明确回滚时可以在修正根因后重新执行。诊断为 `outcome_unknown` 时，停止同一项目的
Lua、Extract、Translate 和其他写入并保留数据库及 sidecar。当前 CLI 没有独立的只读
`status` 或事务恢复命令；不能为了“观察”再运行会取得写事务的 Lua，也不能手工编辑库。
现行公开接口无法确认的现场必须作为产品能力限制报告。
