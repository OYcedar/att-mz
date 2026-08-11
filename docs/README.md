# ATT 文档总入口

这里描述当前发行版 ATT 的行为。指南负责判断现在该做什么、该读哪些规格；规格负责
命令、格式、状态、错误和恢复事实。执行任何项目操作前，先把当前任务路由到的指南和
规格完整读完。

只使用与实际 `att.exe` 同一发行目录中的文档。阶段、项目类型、输入或诊断变化后，重新
从本页选择；不要拿其他安装、源码仓库、旧对话或记忆补写规则。

## 1. 按当前任务进入

| 当前任务 | 阅读顺序 |
| --- | --- |
| 调查新游戏、选择项目、建立完整翻译任务 | [翻译项目指南](guides/translation-project.md) → 对应引擎入口 → 当前阶段规格 |
| 从完整游戏原文制作术语表 | [游戏术语表制作 Skill](../skills/extract-game-terminology/SKILL.md) → [术语规格](translation/terminology.md) |
| 继续旧任务且当前状态明确 | 唯一任务清单 → [翻译项目指南](guides/translation-project.md)的当前阶段 → 对应规格 |
| 不知道旧任务停在哪里 | 唯一任务清单 → [诊断与恢复指南](guides/diagnosis-and-recovery.md) → 权威状态所属规格 |
| 命令出现失败、Partial、Unavailable、取消、警告或结果未知 | [诊断与恢复指南](guides/diagnosis-and-recovery.md) → 当前阶段规格 → 相关公共或运行时规格 |
| 调查遗漏、审校质量、人工或 agent 补译、定点修订 | [全量验收指南](guides/acceptance.md) → [Manual](manual/README.md)；需要批量上下文或复杂数据库操作时再读 [Lua](lua/README.md) |
| WriteBack、外部转换、部署或最终交付 | [全量验收指南](guides/acceptance.md) → 对应 WriteBack 规格 → [目录发布](runtime/directory-publishing.md) |
| 发行包缺文件、配置或 Prompt 来源不明 | [发行物规格](runtime/distribution.md)与[配置规格](runtime/configuration.md)；解决前不执行项目命令 |
| 长期、跨会话或多人任务 | [任务材料规范](guides/task-artifacts.md)；确需新清单时再读[任务清单模板](guides/task-list-template.md) |

## 2. 先选择项目，不要把复杂内容直接交给 Generic

同一游戏可以由一个或多个项目共同处理，但一段文本只能有一个项目所有者。

### MV/MZ 游戏

先完整读取 [RPG Maker 项目入口](rpg-maker/README.md)、
[Extract 规格](rpg-maker/extraction.md)和 [Rules 规格](rpg-maker/rules.md)，按
[翻译项目指南](guides/translation-project.md#31-mvmz-必须先走原生能力判断)执行
Builtin → Rules → Generic 的选择顺序。具体来源、路径、捕获与不能表达的关系只由两份
规格说明。

### 其他来源或 Rules 无法表达的 MV/MZ 来源

读取 [Generic 项目入口](generic/README.md)和 [JSONL 规格](generic/jsonl.md)。外部过程负责
来源格式到 ATT JSONL 的完整映射，也负责把译后 JSONL 放回实际消费者。

### 组合项目

按[翻译项目指南的项目分配方法](guides/translation-project.md#3-逐类选择唯一项目所有者)
记录每类来源、项目、写回方式和消费者。最终按
[组合项目验收](guides/acceptance.md)统一检查遗漏、重叠和加载结果。

## 3. 按阶段进入

| 阶段或能力 | MV/MZ | Generic | 公共或运行时规格 |
| --- | --- | --- | --- |
| 发行与 CLI | — | — | [运行时入口](runtime/README.md)、[CLI](runtime/cli.md)、[配置](runtime/configuration.md)、[发行物](runtime/distribution.md) |
| Init | [MV/MZ Init](rpg-maker/init.md)、[目录发布](runtime/directory-publishing.md) | [Generic Init](generic/init.md) | [SQLite](runtime/sqlite.md) |
| Extract | [MV/MZ Extract](rpg-maker/extraction.md)、[Rules](rpg-maker/rules.md) | [Generic Extract](generic/extraction.md)、[JSONL](generic/jsonl.md) | [语言](translation/language.md)、[SQLite](runtime/sqlite.md) |
| Translate 准备 | [MV/MZ Translate](rpg-maker/translation.md) | [Generic Translate](generic/translation.md) | [公共翻译入口](translation/README.md) |
| 模型请求与结果 | 对应 Translate 规格 | 对应 Translate 规格 | [TaskBlock](translation/task-planning.md)、[Prompt](translation/prompts.md)、[HTTP](runtime/chat-completions.md)、[任务记录](translation/task-records.md) |
| 人工或 agent 查询与修订 | [Manual](manual/README.md) | [Manual](manual/README.md) | [Lua](lua/README.md)、[SQLite](runtime/sqlite.md)、[验收指南](guides/acceptance.md) |
| WriteBack | [MV/MZ WriteBack](rpg-maker/write-back.md) | [Generic WriteBack](generic/write-back.md) | [排版规则](translation/write-back-layout-rules.md)、[目录发布](runtime/directory-publishing.md) |
| 验收与交付 | [全量验收指南](guides/acceptance.md) | [全量验收指南](guides/acceptance.md) | 实际外部转换和消费者说明 |

## 4. 按观察结果进入

进程终态与 Translate 业务结果是两个不同维度：

| 观察到的事实 | 先读什么 |
| --- | --- |
| `succeeded` | [诊断与恢复指南](guides/diagnosis-and-recovery.md#4-按进程终态判断)，再确认本阶段具体业务结果 |
| `failed` | 同上，并读取产生失败的阶段规格 |
| `cancelled` | [诊断与恢复指南的取消分支](guides/diagnosis-and-recovery.md#43-cancelled) |
| `recovery_required` | [诊断与恢复指南](guides/diagnosis-and-recovery.md#44-recovery_required) → 按诊断 operation 与恢复路径读取所属阶段规格；只有目录发布受管路径再读目录发布，SQLite 再读 SQLite |
| `outcome_unknown` | [诊断与恢复指南](guides/diagnosis-and-recovery.md#45-outcome_unknown)；停止新的写入和重跑 |
| Translate Complete、Partial 或 Unavailable | [诊断与恢复指南的 Translate 分支](guides/diagnosis-and-recovery.md#64-translate)与对应引擎 Translate 规格 |
| Rules 跳过警告或 owner 部分提交 | [诊断与恢复指南的 Extract 分支](guides/diagnosis-and-recovery.md#63-extract)与 [Rules](rpg-maker/rules.md) |
| 译后 QA、WriteBack 候选或发布问题 | [诊断与恢复指南的 WriteBack 分支](guides/diagnosis-and-recovery.md#66-writeback-与目录发布) |
| 日志、任务记录或终端呈现失败 | [项目日志](runtime/project-log.md)与[诊断与恢复指南](guides/diagnosis-and-recovery.md#67-sqlite-与可观测性) |

退出码 `0`、输出目录存在、日志写有成功、某次 Translate Complete 或某个项目完成，都不能
单独证明整个游戏翻译已经完成。

## 5. 完整规格索引

### 工作指南

- [翻译项目指南](guides/translation-project.md)
- [诊断与恢复指南](guides/diagnosis-and-recovery.md)
- [全量验收指南](guides/acceptance.md)
- [任务材料规范](guides/task-artifacts.md)
- [任务清单模板](guides/task-list-template.md)

### RPG Maker MV/MZ

- [项目入口](rpg-maker/README.md)
- [Init](rpg-maker/init.md)
- [Extract](rpg-maker/extraction.md)
- [Rules](rpg-maker/rules.md)
- [Translate](rpg-maker/translation.md)
- [WriteBack](rpg-maker/write-back.md)

### Generic

- [项目入口](generic/README.md)
- [JSONL](generic/jsonl.md)
- [Init](generic/init.md)
- [Extract](generic/extraction.md)
- [Translate](generic/translation.md)
- [WriteBack](generic/write-back.md)

### 公共翻译能力

- [Manual TOML 人工补译](manual/README.md)
- [公共翻译入口](translation/README.md)
- [语言](translation/language.md)
- [术语](translation/terminology.md)
- [Placeholder](translation/placeholders.md)
- [WriteBack 排版规则](translation/write-back-layout-rules.md)
- [TaskBlock 规划](translation/task-planning.md)
- [Prompt 与模型协议](translation/prompts.md)
- [模型任务记录](translation/task-records.md)

### 数据库与运行环境

- [Lua](lua/README.md)
- [运行时入口](runtime/README.md)
- [CLI](runtime/cli.md)
- [配置](runtime/configuration.md)
- [Chat Completions](runtime/chat-completions.md)
- [SQLite](runtime/sqlite.md)
- [项目日志](runtime/project-log.md)
- [目录发布](runtime/directory-publishing.md)
- [发行物](runtime/distribution.md)
