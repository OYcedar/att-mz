# ATT 文档

这里描述 ATT 当前实现遵守的产品行为。命令、格式、状态和恢复方式以相应现行规格为
准；指南帮助操作者选择正确的流程。

## 从哪里开始

- [翻译项目工作指南](guides/translation-project.md)：带你调查游戏、选择 MV/MZ 或
  Generic、组合多个独立项目，走完提取、翻译、写回和验收。
- [任务材料规范](guides/task-artifacts.md)：长期任务需要的清单、证据、备份和协作材料。
- [任务清单模板](guides/task-list-template.md)：任务需要持续执行或多人协作时，从这里
  复制使用。

## 三种项目

- [RPG Maker MV/MZ](rpg-maker/README.md)
  - [Init](rpg-maker/init.md)
  - [Extract](rpg-maker/extraction.md)
  - [Rules](rpg-maker/rules.md)
  - [Translate](rpg-maker/translation.md)
  - [WriteBack](rpg-maker/write-back.md)
- [Generic](generic/README.md)
  - [JSONL](generic/jsonl.md)
  - [Init](generic/init.md)
  - [Extract](generic/extraction.md)
  - [Translate](generic/translation.md)
  - [WriteBack](generic/write-back.md)

## 公共翻译能力

- [公共翻译能力导航](translation/README.md)
- [语言](translation/language.md)
- [术语文件](translation/terminology.md)：ATT 接受、校验和使用的格式与行为
- [制作游戏术语表](../skills/extract-game-terminology/SKILL.md)：从游戏或文本中发现、
  筛选、定译和验收术语，不绑定 ATT 格式
- [Placeholder](translation/placeholders.md)
- [TaskBlock 规划](translation/task-planning.md)：完整语境、稳定装箱和临时 ID
- [Prompt 与模型协议](translation/prompts.md)
- [模型任务记录](translation/task-records.md)

## Lua 与运行环境

- [原子数据库 Lua](lua/README.md)
- [运行时导航](runtime/README.md)
- [CLI](runtime/cli.md)
- [配置](runtime/configuration.md)
- [Chat Completions](runtime/chat-completions.md)
- [SQLite](runtime/sqlite.md)
- [项目日志](runtime/project-log.md)
- [目录发布](runtime/directory-publishing.md)
- [发行物](runtime/distribution.md)
