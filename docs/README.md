# ATT 文档

本目录说明 ATT 当前已经确定的产品行为和外部接口。现行规格定义产品承诺，指南说明如何
调查项目、选择功能和验证文件或配置。每份文档都应能够独立阅读和审查。

## 各类资料分别负责什么

- 标题标明“现行规格”的文档完整定义当前产品行为与外部接口。其他说明与现行规格冲突
  时，以现行规格为准，不能要求使用者从实现中猜测另一套规则。
- 指南说明调查方法、功能选择依据和验证方法，不重复定义现行规格中的命令、字段、
  schema 或错误语义。
- 真实项目材料、产品说明和验证记录用于确认事实与功能是否成立。

同一项产品规则只在一个位置完整说明，其他文档通过链接引用。说明缺失或互相冲突时，
停止猜测并报告具体位置。

## 当前产品范围

ATT 当前实现的游戏领域只有 **RPG Maker**，该领域目前只包含 **MV** 与 **MZ** 两种
受支持版本和目录布局。CLI、工作区与项目日志中的 `mv | mz` 是 RPG Maker 域内身份；
它们不与 RPG Maker 并列，也不表示 XP、VX、VX Ace 等其他版本已经受支持。

## 文档导航

### RPG Maker 项目调查与配置制作

- [调查与机制选择指南](rpg-maker/README.md)：从游戏运行时实际读取的位置确认项目事实、
  文本位置、Builtin/Rules/Lua 能力边界、覆盖证据和高级数据库接口边界。
- [规则文件现行规格与编写指南](rpg-maker/rules.md)：MV 姓名映射、Extract Rules 与
  Placeholder Rules 的契约、示例和单项验证。
- [术语文件现行规格与制作指南](rpg-maker/terminology.md)：从真实语料提炼稳定术语，
  并验证当前 Terminology TOML。
- [Prompt 资源与模型协议现行规格及编写指南](rpg-maker/prompts.md)：Prompt locale、
  模板、模型消息、响应信封、JSON wire、ID、ATT token 与协议失败时的处理方式。
- [初始化现行规格](rpg-maker/init.md)
- [文本提取现行规格](rpg-maker/extraction.md)
- [翻译现行规格](rpg-maker/translation.md)
- [翻译任务记录现行规格](rpg-maker/task-records.md)
- [写回现行规格](rpg-maker/write-back.md)
- [Lua 现行规格](rpg-maker/lua.md)
- [Lua Cookbook](rpg-maker/lua-cookbook.md)
- [当前示例索引](rpg-maker/examples/README.md)

### 共享运行能力

- [配置编写与运行能力导航](runtime/README.md)
- [生产配置现行规格](runtime/configuration.md)
- [运行时与 CLI 现行规格](runtime/cli.md)
- [普通项目日志现行规格](runtime/project-log.md)
- [Chat Completions 现行规格](runtime/chat-completions.md)
- [SQLite 运行时现行规格](runtime/sqlite.md)
- [Windows 文件能力与可恢复目录发布现行规格](runtime/directory-publishing.md)
