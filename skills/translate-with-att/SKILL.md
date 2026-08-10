---
name: translate-with-att
description: 使用 ATT 调查、建立、继续、诊断、修订、写回和验收 RPG Maker MV、MZ、Generic 或组合式游戏翻译。适用于用户明确要求使用 ATT、提供 ATT 项目，或要求处理 Init、Extract、Rules、Translate、Manual、Lua、WriteBack、运行错误和恢复。
---

# 使用 ATT 翻译游戏

本 Skill 只组织执行。命令、格式和状态以本次 `att.exe` 同目录的现行文档为准；随 Skill
程序负责一次调查、审核材料、译前检查、译后 QA、运行观察和字体处理。

## 先绑定发行与范围

1. 确认实际 `att.exe`、发行根和 cwd，读取同目录 `README.md`、`docs/README.md`、
   `docs/guides/translation-project.md`。
2. 失败、Partial、Unavailable、取消或状态不明时读取
   `docs/guides/diagnosis-and-recovery.md`；WriteBack 和交付读取
   `docs/guides/acceptance.md`。
3. 记录游戏版本、补丁、MOD、语言、包含范围、排除范围和最终消费者。不同 ATT 项目不得
   重复拥有同一位置。

不要删除、重建或修改已有 ATT 项目材料来“重新开始”。恢复必须依据当前数据库、日志、
Manual、审核材料和发行文档的现有事实。

## 一次调查并确定所有者

先确认 Python 3.11+。Survey 是对本次游戏来源的一次性调查。程序默认拒绝覆盖；
只有确认旧输出可替换时才加 `--replace`。

```powershell
python <Skill>\scripts\rpg_maker_survey.py scan --game <完整游戏安装根> --output <工作目录>\survey
```

调查后先处理未知运行消费者，即尚不知道哪个程序、插件或场景会读取该文本。必须在
所有权决定前，用隔离副本上的 `inspect_nwjs_runtime.py observe` 确认实际消费；无法访问时保持
`unresolved`，不得先分配所有者再补调查。

读取 `review-groups.jsonl`（按同一判断合并的紧凑关系组），在决定文件中只引用自然
`group_id` 或 `candidate_id`：

```json
{"target":"group:group-000001","owner":"rules","reason":"..."}
```

`owner` 只取 `rules`、`generic`、`exclude` 或 `unresolved`。拆组时只写成员决定。Generic
必须逐项写齐工具要求的七项证据；文件存在、疑似显示或源码出现引用都不够。
只有需要拆组时才导出该组的完整成员，不打开整份 `locations.jsonl`：

```powershell
python <Skill>\scripts\rpg_maker_survey.py members --survey <工作目录>\survey --group-id <group_id> --output <组成员.jsonl>
```

游戏路径和语言已固定，且 Init 的来源复制不会与当前磁盘任务争用时，可在 Agent 审核关系组时
并行执行 Init；否则按顺序执行。Extract 必须等待两者都完成。

```powershell
python <Skill>\scripts\rpg_maker_survey.py finalize --survey <工作目录>\survey --decisions <决定.jsonl> --output <工作目录>\plan
```

`coverage.json` 的 `complete=false` 是正常的待审核结果，补齐决定后对同一来源重新 finalize。
完成的计划提供 `dialogue-rules.toml`、`rules.toml`、逐规则 manifest、Unit 投影和预期所有权。
MV 姓名 wrapper 不因外形建立全局规则；未证明的 wrapper 由译前检查按精确自然 ID 审核。

已批准 Generic 来源的 ATT 输入在 `plan/generic/input/`，精确来源映射在
`plan/generic/manifest.json`。它们只覆盖已经审核的位置。随包工具不会把 Generic 译文写回
游戏；必须使用任务中已确认的外部消费过程，并在隔离副本中验证。

## Extract、所有权和译前准备

按发行文档 Init，并用计划中的 dialogue/Rules 执行 Extract。然后分别导出完整所有权和
本轮完整 Manual：

```powershell
att <mv或mz> ownership export --name <项目名> <ownership.jsonl>
att <mv或mz> manual export --name <项目名> --selection all <final-manual.toml>
python <Skill>\scripts\rpg_maker_survey.py audit --survey <工作目录>\survey --plan <工作目录>\plan --ownership <ownership.jsonl> --output <ownership-audit.json>
```

`audit` 的问题不阻止 Translate，但 `complete=false` 时不能宣称来源覆盖完整。不要从 Manual
ID 前缀猜 owner，也不要读取 SQLite 猜映射。
所有权审计完成后，把 `final-manual.toml` 固定为本轮语料；后续如果改动 Extract 或所有权，必须
废弃基于它生成的译前检查和术语作业。

立即按 `skills/extract-game-terminology/SKILL.md` 启动 Formic（从完整语料批量找术语候选的外部工具）。
网络等待期间完成以下独立工作：

- 运行 Preflight（Translate 前的 Placeholder 候选检查），审核 `preflight:<candidate_id>` 后生成精确规则；
- 如需修改前比较字体或只读调查，用 `manage_rpg_maker_fonts.py inspect`；
- 用 `summarize_att_run.py` 汇总已经结束的 ATT 日志，不读取仍在写入的日志。

```powershell
python <Skill>\scripts\translation_preflight.py --manual <final-manual.toml> --survey <工作目录>\survey --coverage <工作目录>\plan\coverage.json --output <工作目录>\preflight
python <Skill>\scripts\translation_preflight.py --manual <final-manual.toml> --survey <工作目录>\survey --coverage <工作目录>\plan\coverage.json --decisions <placeholder-decisions.jsonl> --output <工作目录>\preflight --replace
```

ATT 的 MV/MZ 内建控制符由 ATT 默认规则负责。普通未知外形、语义、术语、语言比例和布局只
进入 Review；不得把它们升级成拒绝译文的强规则。原文固定空槽和非空槽结构仍由 ATT 校验。
各 Python 助手只负责自己的调查、译前检查、术语、QA、运行观察、字体或日志输出；Agent 用显式产物
安排顺序，不增加通用流水线框架，也不让助手互相调用。

## Translate 与一次集中返修

术语使用 `skills/extract-game-terminology/SKILL.md` 和其中唯一入口 `terminology_job.py`。
资源文件名、资源路径、内部键和普通短语不进入术语，也不写入 `allowed_terms`。
译前检查和术语都定稿后再运行 Translate。Translate 持有项目租约（防止同一项目两个 ATT 命令同时
改状态的独占锁）；模型等待期间禁止对同一项目运行 export、Manual 或 WriteBack。只做不读写该项目的
隔离副本准备等独立工作；没有这类工作就等待，不启动并行命令试探。

按现行 Translate 规格运行后导出全部当前状态，再做一次静态 QA（不启动游戏的译后检查）：

```powershell
att <mv或mz或generic> translation export --name <项目名> <translations.jsonl>
python <Skill>\scripts\translation_qa.py scan --translations <translations.jsonl> --survey <工作目录>\survey --terminology <terminology.toml> --output <工作目录>\qa
```

Generic 项目另传 `--generic-manifest <工作目录>\plan\generic\manifest.json`。首次静态 QA 不传 WriteBack 和
NW.js 报告；它们只在最终合并 QA 时传入。`qa_status` 只取 `clean`、`needs_review`、
`unverified`；发现多少 Review 都不会拒绝已有结构合法译文。

需要集中返修时只输出自然 ID，再让 ATT 从当前数据库预填 Manual：

```powershell
python <Skill>\scripts\translation_qa.py manual --scan <工作目录>\qa --output <revision-ids.jsonl>
att <mv或mz或generic> manual export --name <项目名> --ids <revision-ids.jsonl> <revision.toml>
att <mv或mz或generic> manual apply --name <项目名> <revision.toml>
```

编辑 `revision.toml` 后默认直接 apply；apply 会在单个事务内执行与 check 相同的结构校验，失败时不修改
任何条目。只在需要事先试检或单独诊断 TOML 时运行 `manual check`。通常集中修改一次，不重复调用
模型修正已定位的 Review。

apply 成功后重新导出当前译文并再做一次静态 QA；确认本轮修改结果后才执行 WriteBack：

```powershell
att <mv或mz或generic> translation export --name <项目名> <translations-after-manual.jsonl>
python <Skill>\scripts\translation_qa.py scan --translations <translations-after-manual.jsonl> --survey <工作目录>\survey --terminology <terminology.toml> --output <工作目录>\qa-after-manual
```

## WriteBack、最终运行观察和 QA

集中 Manual apply 后执行 WriteBack，再把输出部署到可丢弃的隔离游戏副本。最终字体 apply、smoke
（尝试标准场景的自动检查）和 observe（记录正常游玩中的实际绘制）只在该副本执行：

```powershell
python <Skill>\scripts\inspect_nwjs_runtime.py smoke --game <隔离副本> --output <报告目录> --confirm-isolated-copy
python <Skill>\scripts\inspect_nwjs_runtime.py observe --game <隔离副本> --output <报告目录> --confirm-isolated-copy
```

单个场景无法安全进入时记为 `unsupported` 并继续；未访问场景是 `unverified`。窗口宽度、
溢出、字体回退和英文命中只生成 Review。

默认直接 apply；apply 每次都按隔离副本和最终字体输入重新扫描、生成计划并验证，同时输出检查报告。
只在需要修改前比较或只读调查时先 inspect。每次修改都有事务记录，可按记录 restore：

```powershell
python <Skill>\scripts\manage_rpg_maker_fonts.py apply --game <隔离副本> --font noto-sans-sc --state <字体状态目录> --output <应用.json>
python <Skill>\scripts\manage_rpg_maker_fonts.py restore --game <隔离副本> --state <字体状态目录> --output <恢复.json>
```

字体工具递归处理已确认的完整字体引用，不只修改 MV `gamefont.css` 或 MZ 的单个标准字段。
完成 WriteBack、字体和 NW.js 观察后，使用 Manual 后已经重新导出的 `translation export`、WriteBack 验证报告和运行报告合并
最终 QA。只有运行期报告出现静态 QA 无法看到的新事实时，才追加一轮 Manual、WriteBack 和受影响场景复查；
不得因同一静态问题重复返工。

## 恢复与完成

- Formic 中断时保留同一 input、plan、task、OUT 和 `results`，修正原因后用 `--resume`。
- Python 输入损坏、来源变化或决定冲突时按 stderr 修正；Review 和未验证项保持退出 0。
- 精确来源仍缺运行消费者证据时，使用 `inspect_nwjs_runtime.py observe` 记录实际消费；
  无法访问的场景保持 `unverified`。
- 完成必须覆盖声明范围、所有项目、全部输出、Generic 外部消费和实际场景；一次成功退出、
  `Complete` 或抽样检查都不能单独证明整个游戏完成。
