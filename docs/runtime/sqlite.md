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

## 4. Lua 会话

独立 Lua 使用同一个项目数据库，外面包着一个 `BEGIN IMMEDIATE`；事务边界由 ATT
掌握，脚本只管写逻辑。运行结束后 ATT 先验证 schema、metadata、领域不变量、
`foreign_key_check` 与 `quick_check`，再提交或回滚。完整 API 与 SQL 限制见
[原子数据库 Lua](../lua/README.md)。

Lua 可以建立自己的私有表，ATT 不读取、迁移或解释它们。直接修改 ATT 表属于可信
高级操作：可以做，但结果必须通过最终不变量检查。

## 5. 诊断与日志

SQLite 出错时，诊断保留操作、数据库路径、primary/extended code、事务最终状态和
恢复位置。SQL、参数、结果与游戏正文不进普通项目日志；数据库的恢复和重放另有
依据，项目日志不参与。
