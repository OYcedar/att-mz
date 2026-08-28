---
name: extract-game-terminology
description: 从最终 Manual 的完整游戏原文找出需要统一译法的专有名词，并直接整理为 ATT 术语 TOML；可用随包 Formic 批量处理，也可由 Agent 完成任意单元。
---

# 制作游戏术语表

最终只交付审核后的 `terminology.toml`。候选由 Formic 还是 Agent 产生不影响有效性；有效性只取决于
候选是否逐字存在于最终 Manual、是否确实是需要全游戏统一译法的专有单个名词，以及最终译名是否合适。

长期翻译任务把工作材料留在任务根：临时分片和批处理结果放 `artifacts/work/`，实际采用的
`terminology.toml` 放 `artifacts/rules/`。不要把任务材料写回本 Skill。

## 1. 固定语料

使用最终 Extract 后、首次 Translate 前通过 `manual export --selection all` 得到的完整 Manual。
Manual 改变后，旧候选和旧术语表失效，重新检查完整语料。

资源名、资源路径、控制符、普通词、短语和句子不进入术语表。候选只保留游戏专有的单个名词，
例如角色、地点、组织、物品体系或作品内专有概念。

## 2. 产生候选

Agent 可直接读取完整 Manual，按自然来源和上下文分段检查候选。语料较大时，可以把自然 Scope 写入
`input/`，建立自然编号的 `plan.jsonl`，并让随包 Formic 并发处理：

```powershell
.\formic.exe run --data <作业目录>\input --plan <作业目录>\plan.jsonl --task <作业目录>\task.md --out <OUT>
```

`task.md` 要求每个单元只输出原文候选，一行一个；没有候选时输出 `无`。Formic 失败、未开始或不值得
继续等待的单元，直接由当前 Agent 读取该单元的原始 Scope，按同一要求写入
`OUT/results/<自然单元号>.md`。Agent 也可以从一开始就完成全部单元。不要把 worker 运行档案当作
候选来源，也不要伪造或改写 Formic 的 run summary。

结果是否齐全只按当前计划的自然单元和 `results/<unit>.md` 对照；不以 Formic 状态决定候选能否使用。

## 3. 集中审核

Agent 合并全部单元结果，并用完整 Manual 逐项核对：

- 删除没有逐字原文依据、只出现一次或完全重复的候选；
- 删除资源名、多名词组合、短语、句子、普通词和已有稳定固定译法的词；
- 保留实际可能在不同上下文被译成不同写法、需要全局约束的专有单个名词；
- 结合全部出现位置确定完整、自然、不冲突的简体中文译名。

## 4. 直接写术语 TOML

按[术语文件现行规格](../../docs/translation/terminology.md)直接写 `terminology.toml`：

```toml
[[term]]
term = '星読み'
translation = '观星者'
```

确有多个原文写法需要触发同一译名时再使用非空 `triggers`。没有需要约束的术语时写：

```toml
term = []
```

最终文件由后续 ATT `translate --terms` 读取和严格校验；不要为凑数量保留候选。
