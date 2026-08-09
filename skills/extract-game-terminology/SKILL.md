---
name: extract-game-terminology
description: 使用 ATT 随包 Formic 从最终 Manual 的完整游戏原文并发找出术语候选，再按全游戏出现次数和语境筛成 ATT 术语表。适用于新建、补做或重做游戏翻译术语表。
---

# 制作游戏术语表

最终只交付审核后的 ATT 术语 TOML。Formic worker 只从分配的原文中找候选；全语料统计、
去重、专有性判断和定译由随包程序与 Agent 完成。

## 1. 建立作业

术语必须来自最终 Extract 后、首次 Translate 前的完整 Manual。先按当前 ATT 文档导出
Manual，再确认 Python 3.11+ 并运行：

```powershell
python <Skill>\scripts\prepare_formic_job.py --manual <final-manual.toml> --output <作业目录>
```

程序建立：

- `input/*.md`：不可拆分的自然 Scope 文件；
- `plan.jsonl`：只把同一来源中相邻的小 Scope 装入同一单元；
- `task.md`：所有 worker 共用的最短任务说明；
- `packing-evidence.json`：实际 Formic 文件头与 Markdown 开销、目标字符数、来源连续段和
  每个超大 Scope 的自然位置。

普通单元以约 24,000 个实际渲染字符为目标。Scope 不拆；单个 Scope 超过目标时独占一个
单元并写入证据。单元很多不等于装箱失败，先看 `packing-evidence.json` 中不可跨越的来源
边界和超大 Scope。资源名、资源路径、控制符、短语、句子和普通词都不是术语候选。

## 2. 运行或继续 Formic

在 Formic 发行目录运行，让它读取当前活动 `config.toml`。并发默认取配置；只有服务的真实
限制或测量证据要求时才显式传 `--concurrency` 或 `--config`。

```powershell
.\formic.exe run --data <作业目录>\input --plan <作业目录>\plan.jsonl --task <作业目录>\task.md --out <OUT>
```

中断、额度耗尽或部分失败后，修正直接原因，保留完全相同的 input、plan、task 和 OUT，
在原命令末尾加：

```powershell
--resume
```

已完成结果位于 `OUT/results/<自然单元号>.md`；每次运行的汇总位于
`OUT/runs/run-N/summary.json`。不要删除 `results`，不要从 workers 运行档案拼候选，也不要
把旧 OUT 与新 plan 混用。

## 3. 核对候选

全部单元都有结果后运行：

```powershell
python <Skill>\scripts\review_formic_candidates.py --manual <final-manual.toml> --plan <作业目录>\plan.jsonl --formic-out <OUT> --output <candidates.json>
```

程序只读当前 `results` 与最新 run summary，校验计划和统计完整性，删除全文不存在、只出现
一次和完全重复的候选，并保存全语料自然位置。缺失时只报告数量、首个缺口和少量示例；按
上一节对同一 OUT 执行 `--resume`。`results` 中除 `output-schema.json` 外出现非自然单元号、
未知扩展或目录时必须先清楚来源，不能静默忽略。

Agent 再逐项删除资源名、多名词组合、短语、句子、普通词、已有稳定译法和没有原文依据的
项目，只保留确实需要统一的游戏专有单个名词，并给出完整中文译名。将审核结果写成
`{"terms":[{"term":"…","translation":"…"}]}`，然后运行：

```powershell
python <Skill>\scripts\write_terminology.py --input <reviewed.json> --output <terminology.toml>
```

程序检查空值、控制字符、重复 term、冲突 trigger 和 TOML 转义。最终仍由 ATT
`translate --terms` 解析和使用。没有需要约束的术语时生成合法空集，不为凑数量保留候选。
