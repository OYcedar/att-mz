# ATT SQLite 现行规格

每个 MV、MZ 或 Generic 项目都使用自己工作区内的 `project.db`。引擎或项目名不同，项目
事实、译文和资源互不共享。

## 1. 当前数据库

数据库只保存当前产品实际使用的事实：

- 项目名、来源位置或来源快照和语言；
- Extract 建立的 Group、Unit、自然顺序和写回关系；
- 自动译文及其当前状态；
- 独立保存的人工译文快照；
- 能唯一绑定自然 Unit、但违反强不变量的 Rejected 候选及原因；
- 当前术语、Placeholder 和最近成功 Profile；
- MV/MZ 当前 Builtin/Rules 选择与写回所需资源。

项目只接受当前代码声明的精确 schema。普通命令可以检查当前结构是否完整，但不识别业务
schema 版本，不检测旧格式，不迁移，也不提供兼容 view、别名或双读双写。无效数据库只按
当前项目损坏处理。

Generic 不复制外部 JSONL。外部 JSONL、去重族、代表项和译文历史都不属于项目数据库。

## 2. 自动译文与人工译文

自动译文继续保存在当前 Unit 表中，并与自动状态一起出现或一起为空。自动状态只绑定
正文针对的当前原文、完整实际 Group 来源语境、项目语言对、位置、角色和写回结构；不绑定
Placeholder。只有与这些事实精确匹配的当前适用性指纹才进入候选，随后 Translate、
Manual 和 WriteBack 各自在消费入口重新执行当前 Placeholder 与结构强验收。术语、Prompt、
Profile、Client、模型参数和语言检查阈值只影响未来请求，不改变既有正文的适用性。

人工译文保存在无外键的引擎专用表中，不与自动译文共用正文槽。记录保存内部位置、最后
一次可读 ID、`fixed|free`、原文数组、译文数组和内部适用性指纹。没有外键是有意设计：
Extract 删除或重建当前位置时，旧人工正文仍可保留并供高级 Lua 查看。

人工译文是否当前，只由对应位置、实际写回结构和填写时的项目语言对决定：

- Generic：逻辑 Group/Unit、所属文件、Group kind、正文形状和原文；
- RPG Maker：内部位置、Group kind、Unit 角色、写回 recipe、正文形状和原文。

完整 Group 语境、相邻文本、术语、Placeholder 配置、Prompt、Profile 和 Client 不参与人工
译文失效。位置不存在，或上述实际结构、原文或语言对变化时，记录成为过期；
正文不会被静默删除。
以后条件重新匹配时，同一记录可以再次成为当前。

当前人工译文优先于自动译文。Manual apply 或 `ctx.translation.set` 写入人工记录时，只清除
同一 Unit 的自动译文和 Rejected 候选。Translate 跳过当前人工译文，模型结果提交也不能
覆盖它。当前验收契约改变后，数据库重新读取发现既有人工或自动正文违反强不变量时，正文
和原来源转入 Rejected，原 Current 在同一事务中失效。WriteBack 按“当前人工译文、当前
自动译文、原文”的顺序选择正文；Rejected 不参与 WriteBack。

RPG Maker Rejected 的 `planning_state` 与自动正文状态使用同一当前适用性指纹，绑定项目
语言对、Unit 的位置/角色/recipe、源文/来源上下文和完整 Group 来源语境。`readable_id`
只用于展示，不能证明候选适用。语言或 Extract 来源事实变化时保留同一自然 Unit 的候选，
旧事实下的候选不预填；事实恢复后同一有效候选可以重新成为当前。

语言对或兄弟文本导致的 Group 语境变化使自动正文不再适用时，正文和状态保留在同一 Unit，
但不作为 Current 导出、模型语境或 WriteBack 输入；绑定事实恢复后，原状态可以重新
匹配。Translate 在替代候选通过验收后，使用当前快照
作为 CAS 条件原子覆盖旧正文；请求失败、取消、额度不足或提交冲突都不会先删除它。

## 3. 事务边界

只读规划和 Manual export/check 使用一致的只读快照。普通写操作使用与业务原子范围一致的
短事务和准备语句：

- Generic Extract 整体原子替换当前内容视图；
- MV/MZ Extract 的每个 owner 独立提交；
- Translate 准备阶段原子处理资源与确定的强不变量失效，不在模型请求前删除待替换
  正文；每个模型任务按自然顺序独立提交；
- Manual apply 在一个写事务中重新检查整份 TOML，任一错误使修改为零；
- WriteBack 只读项目数据库，输出提交由目录发布器负责。

提交使用当前快照比较，避免并发命令静默覆盖新状态。SQLite busy 时等待并响应取消，不用
固定本地容量拒绝合法工作。

创建新项目数据库或建立数据库快照时，ATT 从原子占有目标路径开始，持续持有不共享删除
权限的主文件句柄和已固定的父目录链，直到初始化或 online backup 结束并完成最终路径身份
复核。期间目标不能被删除、重命名或替换，外来 SQLite 不能进入本次写入范围。失败清理只按
本次确认的物理身份删除候选；作用结果无法确认时按结果未知处理。

## 4. Raw Lua 可见的当前表

本节只服务使用者主动选择的 `ctx.db` 低级接口。普通补译和高级 Lua 使用可读 ID，不要求
理解以下内部键。

Generic 主要表：

| 表 | 当前职责 |
| --- | --- |
| `generic_project` | 项目、来源、语言、最近 Extract 与 Profile |
| `generic_file` | 当前 JSONL 文件与自然顺序 |
| `generic_group` | Group、文件、kind、顺序和上下文状态 |
| `generic_unit` | Unit、原文、自动译文和自动状态 |
| `generic_manual_translation` | 独立人工译文快照与内部适用性 |
| `generic_rejected_translation` | 当前 Rejected 候选、来源、确定原因和验收状态 |
| `translation_resource` | 当前术语与 Placeholder |

`generic_unit` 的自动正文使用 `translation` 与 `translation_state`；人工正文只使用
`generic_manual_translation`。Generic 人工条目始终是 `free`，人工表不重复保存
`translation_type`；Manual 和 Lua 在读取时直接使用这一固定含义。

MV/MZ 主要表：

| 表 | 当前职责 |
| --- | --- |
| `metadata` | 项目、语言与来源快照 |
| `rpg_maker_asset_owner_state` | Builtin/Rules 当前来源状态 |
| `rpg_maker_project_definition` | 当前 MV/MZ 项目定义 |
| `rpg_maker_translation_resource` | 当前术语与 Placeholder |
| `rpg_maker_text_group` | 当前 Group、自然顺序、kind 与写回 recipe |
| `rpg_maker_text_unit` | 当前 Unit、owner、Rules 自然规则序号、原文、上下文、自动译文和自动状态 |
| `rpg_maker_manual_translation` | 独立人工译文快照与内部适用性 |
| `rpg_maker_rejected_translation` | 当前 Rejected 候选、来源、确定原因和验收状态 |
| `rpg_maker_mutation_claim` | 写回修改范围 |

人工表没有外键。Raw SQL 可以直接读取或破坏这些表；ATT 不保证被修改后的数据库仍满足
普通命令要求。RPG Maker 人工表保留 `translation_type`，因为失去当前位置后仍需区分
`fixed` 与 `free`，无法再从当前 Unit 推导。当前 DDL 以源码为准，不建立另一个 schema
版本或迁移文档。

`rpg_maker_text_unit.rule_number` 保存 Extract Rules TOML 中从 1 开始的自然规则序号；
Builtin Unit 必须为 `NULL`，Rules Unit 必须为正整数。这个事实供 MV/MZ Manual 所有权导出
精确说明来源，不作为公开 ID，也不替代 Unit 的内部身份。新增该列后，旧结构的 MV/MZ
项目不能在原工作区直接重跑 Init：先在 ATT 之外备份并移走或清理整个旧项目工作区，再用
当前程序新建项目并执行 Init、Extract。ATT 不迁移或兼容旧项目数据库。

## 5. Lua 数据库连接

Lua 命令在项目租约内打开同一个 `project.db`，连接从 autocommit 开始。脚本自行决定是否
使用显式事务：

```lua
ctx.db.execute("BEGIN IMMEDIATE")
ctx.db.execute("UPDATE ...")
ctx.db.execute("COMMIT")
```

- autocommit statement 成功后立即保留；
- 已显式 COMMIT 的修改不会因脚本稍后失败而撤销；
- 失败、取消或 panic 只回滚当时仍打开的事务；
- 正常结束时仍有事务未关闭，ATT 报错并回滚该事务；
- raw API 不运行 schema 白名单、业务状态检查、`foreign_key_check`、`quick_check`、自动
  修复或强制备份。

Generic Lua 直接打开 `project.db`，即使 ATT 表已经被删除也能再次运行。Raw API 可以执行
DML、DDL、PRAGMA 和显式事务，可以关闭外键、制造孤儿关系、写乱码状态、删除数据或表。
只继续拒绝 `ATTACH`、`DETACH`、`load_extension` 和一次调用中的第二条 statement。完整
Lua 契约见 [项目数据库 Lua](../lua/README.md)。

## 6. 公开诊断

SQLite 和事务错误在内部保留足够事实用于控制流，但进入 CLI、普通项目日志和任务记录前，
只呈现：

```text
relation, object, reason, impact, help
```

relation 说明主错误或清理、回滚、丢弃、收尾、关闭、可观测性关系；对象使用项目数据库、
自然路径或当前命令描述；原因说明实际失败；impact 说明对业务状态的影响；help 说明可以修改
什么。
公开输出不保存查询 ID、SQLite primary/extended code、原始数据库行、参数、SQL、内部事务
阶段、数据库随机键或 expected/actual fingerprint。

事务明确回滚时，可以修正原因后重试。已经自动提交或显式提交的 Lua 修改不会被描述成
回滚。提交或目录交换结果确实无法确认时，停止继续写入并保留现场；项目日志只是证据，
不参与补写、回滚或恢复判断。
