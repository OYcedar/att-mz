---
name: extract-game-terminology
description: 从最终 Extract 的完整 Manual 中识别需要全游戏统一译法的专有单个名词，结合全部出现位置定译并生成 ATT 术语 TOML；适用于首次 Translate 前制作或语料更新后重审术语表。
---

# 制作游戏术语表

最终产物是供 ATT `translate --terms` 使用的 `terminology.toml`。Agent 负责全局筛选和定译；
Formic 是大量独立语料分片的可选模型调用工具。

## 1. 固定完整语料

使用最终 Extract 后、首次 Translate 前通过 `manual export --selection all` 得到的完整 Manual。
Manual 更新时，以新语料重新检查候选、出现位置和译名。

术语表集中收录需要全游戏统一译法的专有单个名词，例如角色、地点、组织、称号、物品体系和
作品内专有概念。资源路径、内部键、控制符、普通短语和完整句子继续由来源规则、Placeholder
或正文翻译负责。

## 2. 生成分片候选

Agent 可以按自然章节、地图、角色或剧情 Scope 直接检查完整 Manual。每个 Scope 只输出原文候选，
一行一个；没有候选时记录空结果。

语料较大且各 Scope 能够独立判断时，按随包
[Formic 使用说明](../../tools/formic/README.md)建立输入、自然 Scope 计划和共同任务说明。Formic
按其 TOML 配置调用模型并发布每个 Scope 的候选结果。Agent 直接处理尚无结果的 Scope，然后按
本次 Scope 清单收齐候选并进入全局审核。

## 3. 全局审核与定译

合并全部候选后，在完整 Manual 中检查每个候选的所有出现位置：

1. 确认候选逐字存在于可翻译原文；
2. 确认它表达一个完整的游戏专有名词；
3. 确认多次出现或跨上下文使用时需要统一译法；
4. 结合角色、世界观和全部语境确定自然、完整的简体中文译名；
5. 合并同一原文身份，保留确实需要共同触发同一译名的原文写法。

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

完成后保存 `terminology.toml` 的实际路径，并把它交给后续 ATT Translate。
