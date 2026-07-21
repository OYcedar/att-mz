# ATT 文档

本目录同时保存使用判断和现行技术契约。两者的权重不同：指南帮助读者寻找事实、选择
机制和组织验证；标题标明“现行规格”的文档定义当前实现必须遵守的外部契约。指南、
代码或测试与现行规格冲突时，以现行规格为准，并应修复实现和验证材料、消除冲突，
而不是要求外部使用者从源码中猜测另一套解释。

## 当前产品范围

ATT 当前实现的游戏领域只有 **RPG Maker**，该领域目前只包含 **MV** 与 **MZ** 两种
受支持版本和目录布局。CLI、工作区与审计中的 `mv | mz` 是 RPG Maker 域内身份；它们
不与 RPG Maker 并列，也不表示 XP、VX、VX Ace 等其他版本已经受支持。

## 文档导航

### RPG Maker

- [调查与决策指南](rpg-maker/README.md)：面对未知 MV/MZ 游戏时，如何取得初始化事实、
  定位真实文本载体、判断 Builtin/Rules 是否足够，以及如何验证无 Lua 的翻译和写回。
- [规则编写指南](rpg-maker/rules.md)：根据真实文本载体编写 MV 姓名投影、Extract Rules
  与 Placeholder Rules。
- [术语表制作指南](rpg-maker/terminology.md)：从结构化字段与上下文提炼可复用术语，
  并写成当前 Terminology TOML。
- [初始化现行规格](rpg-maker/init.md)
- [文本提取现行规格](rpg-maker/extraction.md)
- [翻译现行规格](rpg-maker/translation.md)
- [写回现行规格](rpg-maker/write-back.md)
- [可信 Lua 现行规格](rpg-maker/lua.md)

### 共享运行能力

- [配置编写指南](runtime/README.md)：从当前命令反推实际消费的配置，并验证路径、资源、
  Profile、Prompt 与 Client 选择。
- [生产配置](runtime/configuration.md)
- [运行时与 CLI](runtime/cli.md)
- [强审计账本](runtime/audit-log.md)
- [Chat Completions 运行根](runtime/chat-completions.md)
- [SQLite 运行时](runtime/sqlite.md)
- [Windows 文件能力与可恢复目录发布](runtime/directory-publishing.md)

## 阅读方式

不需要为获得“完整流程感”而一次读完所有文档。先从当前用户结果出发，阅读
[RPG Maker 调查与决策指南](rpg-maker/README.md)中相应问题，再进入它链接的现行规格。
已有项目可以直接从提取、翻译、写回或故障诊断进入，不要求为了形式完整而重新执行
此前阶段。

使用者可以读取源码、运行命令、编辑配置、查询或修改项目数据库，并自行编写临时程序。
这些都是解决问题的能力，不是文档强制规定的步骤。权限和副作用范围仍由使用者、运行
环境与当前任务决定。
