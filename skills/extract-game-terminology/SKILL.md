---
name: extract-game-terminology
description: 从最终 Extract 的完整 Manual 中识别需要全游戏统一译法的专有单个名词，结合全部出现位置定译并生成 ATT 术语 TOML；适用于首次 Translate 前制作或语料更新后重审术语表。
---

# 制作游戏术语表

最终产物是供 ATT `translate --terms` 使用的 `terminology.toml`。由随包 Formic 从原文初筛术语候选，再由当前负责翻译的 Agent 完成最终筛选、上下文核对、去重与定译。

## 1. 固定完整语料

确认当前项目的目标语言，使用最终 Extract 后、首次 Translate 前通过 `manual export --selection all` 得到的完整 Manual。Manual 更新时，以新语料重新检查候选与译名。

术语表集中收录需要全游戏统一译法的专有单个名词（角色、地点、组织、称号、物品体系和作品内专有概念）。资源路径、内部键、控制符、普通短语和完整句子由来源规则、Placeholder 或正文翻译处理。

## 2. Formic 初筛候选

1. 按随包 [Formic 使用说明](../../tools/formic/README.md#2-配置模型)执行连接自检。配置缺失或自检未通过时暂停并向使用者报告，待修正后继续，不跳过 Formic 初筛。
2. 准备覆盖完整 Manual 的输入、自然 Scope 计划（按章节、地图、角色或剧情划分）和共同任务说明，要求从所给原文列出术语候选，一行一个；无候选时记录空结果。
3. 核对全部 Scope 均有结果后进入全局审核；未完成的 Scope 按 Formic 使用说明恢复续跑。

## 3. 全局审核与定译

合并全部初筛候选后，由当前 Agent 直接在完整 Manual 中核对每个候选的出现位置和上下文，完成筛选与定译：

1. 确认候选逐字存在于可翻译原文；
2. 确认它表达一个完整的游戏专有名词，且跨上下文使用时需要统一译法；
3. 结合角色、世界观和语境，按目标语言确定自然、准确的译名；
4. 合并同一实体的不同原文写法。

## 4. 写入 ATT 术语文件

按[术语文件现行规格](../../docs/translation/terminology.md)写入：

```toml
[[term]]
term = '星読み'
translation = '观星者'
```

多个原文写法共同触发同一译名时使用 `triggers`。当前语料没有需要全局约束的术语时写：

```toml
term = []
```

核对最终 `terminology.toml` 无重复且触发词准确后，保存路径并交给后续 ATT Translate。
