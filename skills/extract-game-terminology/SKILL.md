---
name: extract-game-terminology
description: 使用 ATT 随包 Formic 从完整游戏可翻译原文中并发抓取术语候选，再由 Agent 按全游戏出现次数、既有固定译法、单一名词、完整汉化和重复项筛成 ATT 术语表。适用于新建、补做或重做游戏翻译术语表。
---

# 制作游戏术语表

最终只交付精简的 ATT 术语 TOML。Formic worker 只负责从自己的剧情分片里找候选；不要让
worker 去重、统计、定译或生成结构化结果。

## 1. 整理原文

把游戏全部可翻译原文整理到同一个 `input` 目录。剧情正文使用一个或多个 UTF-8 Markdown
文件；界面、物品说明等其他原文也可以放入该目录，供 worker 按需查阅。

按章节、场景或连续剧情自然拆分正文。每个分片通常可以有几千字到一万多字，但不设固定
字数：不要切成缺少语境的小片，也不要为了减少单元把明显过长的多段剧情硬塞在一起。

## 2. 建立最简单的 Formic 作业

一个剧情分片文件对应一个单元，确保所有剧情分片各出现一次：

```json
{"unit":1,"files":["plot/chapter-01.md"]}
{"unit":2,"files":["plot/chapter-02.md"]}
```

`task.md` 只写下面这些要求：

```text
从当前剧情分片直接找出可能在翻译时写法不一致的游戏专有单个名词，只输出原文，一行一个。
只摘录游戏文本中真实出现的内容。不要造词，不要拼接多个名词，不要输出短语、句子、普通词或译文。
不去重、不统计次数、不判断最终是否收录；看到候选就列出。需要理解候选时，可以搜索或读取 input 中的其他文本。
没有候选时输出“无”。
```

不要创建 `result.schema.json`，也不要传 `--output-schema`。在 Formic 所在目录运行，以便它
读取同目录的 `config.toml`。首次使用时填写该配置，并按服务接口设置
`FORMIC_LLM_PROTOCOL` 为 `completions`、`responses` 或 `anthropic`：

```powershell
Set-Location <ATT目录>\tools\formic
.\formic.exe run --data <input绝对路径> --plan <plan.jsonl绝对路径> --task <task.md绝对路径> --out <out绝对路径> --concurrency 60
```

并发不得低于 60；能够承受时可以更高。模型服务不能支持 60 并发时停止并说明，不能私自
降低。

## 3. Agent 统一筛选

全部单元结束后，Agent 读取 `out` 根目录的数字编号 Markdown，不把 `workers` 运行档案当作
候选。逐项直接检查完整游戏可翻译原文：

1. 在全游戏可翻译原文中只出现一次的，删除。
2. 不是一个游戏专有名词，或者包含多个名词、短语、句子的，删除。
3. 已经有固定译法、不需要术语表约束也不会翻译不统一的，删除。
4. 找不到原文依据、由 worker 补写或推测出来的，删除。
5. 对剩余候选去重，只保留确实需要统一译法的最小集合。
6. 为每项确定一个完全汉化的中文译名；译名仍夹带未处理的日文或其他源语言时，修正或删除。

最后按 `docs/translation/terminology.md` 写入严格 TOML，每项只使用 `term` 和
`translation`。不要另外生成候选数据库、证据表、评分、报告、schema 或验证流程。
