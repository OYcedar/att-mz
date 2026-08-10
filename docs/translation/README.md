# 公共翻译能力导航

MV、MZ 和 Generic 各自拥有项目状态与流程，同时复用语义相同的翻译能力。资源仍由每个
项目分别保存；公共配置中的 Profile 定义可以复用，但每个项目独立记录实际选择。

按当前问题读取：

| 当前问题 | 必读规格 |
| --- | --- |
| 源语言、目标语言、源语判断、残留或安全修复 | [语言](language.md) |
| 从完整游戏原文制作术语表 | [游戏术语表制作 Skill](../../skills/extract-game-terminology/SKILL.md) → [术语](terminology.md) |
| ATT 术语文件的格式、匹配、保存或失效 | [术语](terminology.md) |
| 不可改写内容、token、捕获、恢复或重叠 | [Placeholder](placeholders.md)；MV/MZ 同时读 [Rules](../rpg-maker/rules.md) |
| 候选能否保存、强不变量、Review 与 Rejected | [译文候选验收](candidate-validation.md) |
| Unit、Group、Semantic Scope、稳定装箱或临时 ID | [TaskBlock 规划](task-planning.md) |
| System/User、响应 JSON、ID、形状或逐项验收 | [Prompt 与模型协议](prompts.md) |
| 实际请求、原始 Assistant 或最终任务结果 | [模型任务记录](task-records.md) |
| HTTP、超时、限速或运行时有限重试 | [Chat Completions](../runtime/chat-completions.md) |
| Current、Partial、Unavailable 或引擎状态 | 对应 [MV/MZ Translate](../rpg-maker/translation.md)或 [Generic Translate](../generic/translation.md) |
| 人工或 agent 补译、定点修订 | [Manual TOML](../manual/README.md)；需要批量上下文或特殊数据库操作时再读 [Lua](../lua/README.md) |

术语内容的发现、筛选和定译不由 ATT 文件格式决定。ATT 只接收已经确认的术语要求，并按
[术语规格](terminology.md)读取和应用。

处理失败、不完整结果或重复无进展时，先走
[诊断与恢复指南](../guides/diagnosis-and-recovery.md#64-translate)，不能从某一种错误推导
通用重试、换模型或修改全局规则的方案。少量剩余条目优先使用 Manual TOML；只有需要
复杂筛选、批量变换、诊断或特殊数据库修改时才直接使用 Lua。
