# 公共翻译能力导航

MV、MZ 和 Generic 各自拥有项目状态与流程，同时复用语义相同的翻译能力。资源仍由每个
项目分别保存；公共配置中的 Profile 定义可以复用，但每个项目独立记录实际选择。

按当前问题读取：

| 当前问题 | 必读规格 |
| --- | --- |
| 源语言、目标语言、源语判断、残留或安全修复 | [语言](language.md) |
| 从完整游戏语料制作、补全或重做术语表 | [Formic 术语表指南](../guides/formic-terminology.md) → [游戏术语表制作 Skill](../../skills/extract-game-terminology/SKILL.md) → [术语](terminology.md) |
| 已确认 ATT 术语文件的格式、匹配、保存或失效 | [术语](terminology.md) |
| 不可改写内容、token、捕获、恢复或重叠 | [Placeholder](placeholders.md)；MV/MZ 同时读 [Rules](../rpg-maker/rules.md) |
| Unit、Group、Semantic Scope、稳定装箱或临时 ID | [TaskBlock 规划](task-planning.md) |
| System/User、响应 JSON、ID、形状或逐项验收 | [Prompt 与模型协议](prompts.md) |
| 请求、Assistant 响应、JSON 修复或逐 ID 诊断 | [模型任务记录](task-records.md) |
| HTTP、超时、限速或运行时有限重试 | [Chat Completions](../runtime/chat-completions.md) |
| Current、Partial、Unavailable 或引擎状态 | 对应 [MV/MZ Translate](../rpg-maker/translation.md)或 [Generic Translate](../generic/translation.md) |
| 人工或 agent 补译、定点修订 | [Manual TOML](../manual/README.md)；需要批量上下文或特殊数据库操作时再读 [Lua](../lua/README.md) |

术语内容的发现、筛选和定译不由 ATT 文件格式决定。需要从实际游戏制作术语表时，按
[Formic 术语表指南](../guides/formic-terminology.md)使用
[游戏术语表制作 Skill](../../skills/extract-game-terminology/SKILL.md)。Formic 负责完整语料
中的分片候选发现；外部 Agent 负责计划、全局纠错、合并、去重和最终定译，再按 ATT
[术语规格](terminology.md)接入。RPG Maker MV/MZ 的结构化字段检查只补充候选和证据，不能
代替这条主流程。

处理失败、不完整结果或重复无进展时，先走
[诊断与恢复指南](../guides/diagnosis-and-recovery.md#64-translate)，不能从某一种错误推导
通用重试、换模型或修改全局规则的方案。少量剩余条目优先使用 Manual TOML；只有需要
复杂筛选、批量变换、诊断或特殊数据库修改时才直接使用 Lua。
