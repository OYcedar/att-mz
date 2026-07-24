# ATT 文档

本目录保存 ATT 当前产品知识：现行规格定义已经确认的外部契约，指南解释如何调查事实、
选择机制和验证单件工件。文档能够独立阅读和审查，不依赖某个执行入口才能成立。

需要实际推进 RPG Maker 汉化任务时，使用
[translate-rpg-maker-with-att Skill](../skills/translate-rpg-maker-with-att/SKILL.md)；
它根据当前状态选择工作、按需读取本目录中的权威文档，但不重新定义这里的事实。

## 事实源分工与权重

- [AGENTS.md](../AGENTS.md)保存长期产品方向、架构判断方法、知识治理与同步门禁。
- 标题标明“现行规格”的文档拥有当前产品与外部接口契约。指南、代码或测试与现行规格
  冲突时，应修复冲突材料，不要求使用者从实现中猜测另一套契约。
- 指南拥有调查方法、机制选择依据和验证方法，不重复定义现行规格中的命令、字段、
  schema 或错误语义。
- 代码和测试保存已经实现的行为与验证证据，不能反向覆盖已确认的现行契约。
- 代表性真实材料、权威说明和验证记录用于证明领域事实与能力是否成立。
- Skill 只拥有触发、状态路由、任务组织和按需阅读规则，不是产品事实源。

同一事实只由一个位置拥有。其他材料通过链接消费它；发现知识缺口时，先补充其语义
所有者，再更新相关导航、执行入口和验证。

## 当前产品范围

ATT 当前实现的游戏领域只有 **RPG Maker**，该领域目前只包含 **MV** 与 **MZ** 两种
受支持版本和目录布局。CLI、工作区与项目日志中的 `mv | mz` 是 RPG Maker 域内身份；
它们不与 RPG Maker 并列，也不表示 XP、VX、VX Ace 等其他版本已经受支持。

## 文档导航

### RPG Maker 调查与工件知识

- [调查与机制选择指南](rpg-maker/README.md)：从实际消费者确认游戏事实、文本载体、
  Builtin/Rules/Lua 能力边界、覆盖证据和高级数据库边界。
- [规则文件现行规格与编写指南](rpg-maker/rules.md)：MV 姓名投影、Extract Rules 与
  Placeholder Rules 的契约、示例和单件验证。
- [术语文件现行规格与制作指南](rpg-maker/terminology.md)：从真实语料提炼稳定术语，
  并验证当前 Terminology TOML。
- [系统提示词编写指南](rpg-maker/prompts.md)：Prompt locale、模板、模型信封、ATT
  token 与协议失败边界。
- [初始化现行规格](rpg-maker/init.md)
- [文本提取现行规格](rpg-maker/extraction.md)
- [翻译现行规格](rpg-maker/translation.md)
- [写回现行规格](rpg-maker/write-back.md)
- [可信 Lua 现行规格](rpg-maker/lua.md)
- [Lua Cookbook](rpg-maker/lua-cookbook.md)
- [当前示例索引](rpg-maker/examples/README.md)

### 共享运行能力

- [配置编写与运行能力导航](runtime/README.md)
- [生产配置现行规格](runtime/configuration.md)
- [运行时与 CLI 现行规格](runtime/cli.md)
- [普通项目日志现行规格](runtime/project-log.md)
- [LLM 调用审阅档案现行规格](runtime/llm-call-review.md)
- [Chat Completions 运行根现行规格](runtime/chat-completions.md)
- [SQLite 运行时现行规格](runtime/sqlite.md)
- [Windows 文件能力与可恢复目录发布现行规格](runtime/directory-publishing.md)
