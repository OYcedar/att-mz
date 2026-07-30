# ATT SQLite 现行规格

每个 MV、MZ 或 Generic 项目只使用自己工作区内的 `project.db`。不同引擎、不同项目名之间
不共享表、译文或资源。

## 1. 项目数据库

数据库保存当前产品实际需要的项目事实：

- 项目身份、来源路径或来源指纹、语言；
- Extract 后的 Group、Unit、顺序与写回关系；
- Unit 译文和语义状态；
- 当前术语、Placeholder 与最近 Profile；
- MV/MZ 当前 Builtin/Rules 选择与必要的发布交接。

Generic 不保存外部 JSONL 副本、去重族、代表项、译文历史或 kind 注册表。

项目使用严格的当前 schema，不在运行时识别、迁移或兼容旧 schema。数据库不符合当前
schema 时按普通无效项目处理。

## 2. 读取与写事务

只读规划使用一致快照。写操作使用短事务、批量 statement 和准备语句；不得为每个 Unit
建立独立持久事务。

Extract 的同一 owner 或整个 Generic 同步原子提交。Translate 准备阶段原子处理失效和
资源替换，各模型任务按自然顺序独立提交，以保留有效前序进度。WriteBack 只读数据库，
输出发布由目录发布协议负责。

写事务开始、COMMIT、ROLLBACK 和无法确认结果都形成结构化诊断。`busy` 等待响应取消，
不使用固定的本地队列容量拒绝项目。

## 3. 译文状态

目标译文正文与语义状态分开保存。模型任务提交使用 CAS 同时比较当前源事实、旧目标正文
和旧状态，不能覆盖并发修订。

自动状态绑定语言、Prompt、Client、Group 语境、实际术语和 Placeholder。人工 Lua 状态
只绑定来源、Group 语境、语言、结构和实际 Placeholder。各引擎规格决定失效范围。

## 4. Lua 会话

独立 Lua 使用同一个项目数据库和一个外层 `BEGIN IMMEDIATE`。脚本没有事务控制权；运行
结束后 ATT 验证 schema、metadata、领域不变量、`foreign_key_check` 与 `quick_check`，
再提交或回滚。完整 API 与 SQL 限制见[原子数据库 Lua](../lua/README.md)。

Lua 可以建立自己的私有表，但 ATT 不读取、迁移或解释它们。直接修改 ATT 表属于可信高级
操作，必须服从最终不变量检查。

## 5. 诊断与日志

SQLite 错误保留操作、数据库路径、primary/extended code、事务最终状态和恢复位置。SQL、
参数、结果与游戏正文不进入普通项目日志。项目日志不是数据库恢复或重放来源。
