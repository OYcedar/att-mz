---
name: extract-game-terminology
description: 使用 ATT 随包 Formic 从最终 Manual 的完整游戏原文并发找出术语候选，再按全游戏出现次数和语境筛成 ATT 术语表。适用于新建、补做或重做游戏翻译术语表。
---

# 制作游戏术语表

最终只交付审核后的 ATT 术语 TOML。唯一 Python 入口是 `terminology_job.py`；Formic 网络等待
单独记录，不计 Agent 调查、审核和返工时间。

## 1. 建立作业

输入必须是最终 Extract 后、首次 Translate 前用 `manual export --selection all` 得到的完整
Manual。确认 Python 3.11+ 后运行：

```powershell
python <Skill>\scripts\terminology_job.py prepare --manual <final-manual.toml> --output <作业目录>
```

程序生成 `input/`、`plan.jsonl`、`task.md` 和 `packing-evidence.json`。同一来源中相邻的小
Scope 可以装入同一个 Formic unit；Scope 本身不拆，超大 Scope 单独成组。Agent 不手写或
复制 unit 清单。

资源名、资源路径、控制符、短语、句子和普通词都不是术语候选。候选必须是需要在全游戏
统一译法的专有单个名词。

## 2. 运行或继续 Formic

在实际 Formic 发行根运行，使用当前活动配置：

```powershell
.\formic.exe run --data <作业目录>\input --plan <作业目录>\plan.jsonl --task <作业目录>\task.md --out <OUT>
```

需要使用其他位置的配置时显式传 `--config <FILE>`。并发默认取配置；只有服务的真实限制或
测量证据要求时才传 `--concurrency`。

中断、额度问题或部分失败后，保留完全相同的 input、plan、task 和 OUT，修正直接原因，
在原命令末尾加：

```powershell
--resume
```

不要删除 `OUT/results`，不要从 worker 档案拼接候选，也不要把旧 OUT 与新 plan 混用。

## 3. 核对候选

```powershell
python <Skill>\scripts\terminology_job.py review --manual <final-manual.toml> --plan <作业目录>\plan.jsonl --formic-out <OUT> --output <candidates.json>
```

程序读取 Formic 原生 resume 状态和已发布结果，核对每个自然 unit，按完整 Manual 统计出现
次数并删除无原文依据和完全重复的候选。结果缺失时只报告总数、首个缺口和少量样例；回到
上一节对同一 OUT 执行 `--resume`。

Agent 集中审核 `candidates.json`，删除资源名、多名词组合、短语、句子、普通词、已有稳定
译法和没有原文依据的项目。审核文件只保留：

```json
{"terms":[{"term":"原文专名","translation":"完整中文译名"}]}
```

## 4. 生成术语文件

```powershell
python <Skill>\scripts\terminology_job.py finalize --input <reviewed.json> --output <terminology.toml>
```

程序检查空值、控制字符、重复 term、冲突 trigger 和 TOML 转义。最终仍由 ATT
`translate --terms` 解析和使用。没有需要约束的术语时生成合法空集，不为凑数量保留候选。
