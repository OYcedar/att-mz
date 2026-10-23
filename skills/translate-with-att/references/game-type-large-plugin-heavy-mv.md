# 大型、插件密集的 RPG Maker MV 翻译经验

整理数万翻译单元、多插件 MV 项目中常见的混合插件参数、内联姓名控制码、同文件组合写回和批量 QA 处理方法。

## 插件参数混合玩家文字与协议值

同一嵌套参数或 Survey 关系组中，界面标题、说明常与内部编号、资源路径并存：

1. 检查各字段在插件源码中实际由哪些读取、绘制或查找操作使用。
2. 按[调查指南](../../../docs/guides/translation-project.md#2-调查可见文本)用 `rpg_maker_survey.py members` 导出逐候选决定，将显示文本归入 `rules` 或 `generic`，内部键与资源路径归入 `exclude`（相关标题和说明仍可归入同一 Generic 语义组）。
3. 重新运行 `finalize` 并执行 Extract。

## 姓名藏在内联控制码中

原文含 `\N<...>`、`\NC<...>` 等内联姓名控制码时（参见 [RPG Maker 控制符规格](../../../docs/rpg-maker/rules.md#68-内建控制符)）：

1. 追踪活动消息插件的解析逻辑，确认尖括号内确为显示姓名。
2. 若 Extract 已将姓名投影为独立 Speaker，直接修订对应条目；若仍位于正文中，通过 `translation_preflight.py` 选择 `shell_with_text` 保护前后外壳并开放内部姓名翻译。

## 组合写回后字面量位置变化

MV WriteBack 与 Generic 外部转换处理同一文件的不同位置时，前序译文长度变化会改变后续字面量的绝对字符偏移：

- 合并时结合文件、AST 字面量位置、原文、引号和上下文唯一确定替换点，按固定顺序合入隔离副本，并校验最终 JSON 或 JavaScript 语法。

## 静态 QA 候选较多

运行 `translation_qa.py scan` 后，先处理 `qa-summary.json` 中的确定状态（`confirmed_fact`），再按 `review-groups.jsonl` 分类排查启发式候选：

- 区分拟声词、专名保留、误纳入的协议值与真实漏译；
- 误纳入的协议值退回所有权决定设为 `exclude`；需改译的正文通过 `translation_qa.py manual` 导出自然 ID，在 Manual 中集中修订并重新导出复查。
