---
name: translate-with-att
description: 使用 ATT 调查、建立、继续、诊断、修订、写回和验收 RPG Maker MV、MZ、Generic 或组合式游戏翻译，并根据用户实玩反馈处理漏翻、误译、显示问题和后续返修。适用于用户明确要求使用 ATT、提供 ATT 项目，或要求处理 Init、Extract、Rules、Translate、Manual、Lua、WriteBack、运行错误、恢复和实玩反馈。
---

# 使用 ATT 翻译游戏

本 Skill 只组织执行。命令、格式和状态以本次 `att.exe` 同目录的现行文档为准；随 Skill
程序负责一次调查、审核材料、译前检查、译后 QA、运行观察和字体处理。

## 按循环交付

把翻译视为“首次交付 → 用户实玩 → 反馈复查 → 再次交付”的循环，不承诺一次完成全部运行
场景。首次交付优先减少重复扫描、重复确认、无效等待和分散返修；只有缺失事实会明显改变
结果、权限或数据安全时才询问，其他不确定项保留为 Review 或 `unverified`，不要全部提前交给
用户确认。

用户实际游玩后，把场景、原文或自然 ID、看到的问题和期望结果作为新证据。针对这些精确
问题重新做 QA、集中 Manual、WriteBack 和受影响场景复查，可以有第二轮或更多轮；不要为了
避免返工而阻塞首次可用版本。

开始首次调查、恢复中断任务或处理新一轮用户实玩反馈前，必须完整读取
[游戏类型经验索引](references/game-types.md)，按当前游戏的结构选择并完整读取所有适用经验文件；
只读取选中的文件。同一轮已经读完且文件未变化时不重复读取。没有符合类型时直接按本 Skill
主流程执行，不套用相似名称或未经验证的方法。

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

长期或跨会话任务先按 `docs/guides/task-artifacts.md` 建立一个以游戏自然名、版本、范围和
目标语言命名的任务根。本文中的 `<工作目录>` 指该任务根内的 `artifacts/work/`；已经采用的
决定放 `artifacts/decisions/`，实际使用的规则放 `artifacts/rules/`，最终 QA 证据放
`artifacts/qa/`。
本游戏专用且需要保留的辅助脚本放 `artifacts/scripts/`，一次性临时脚本放
`artifacts/scratch/`。禁止把任务脚本、过程文件或临时规则写入 Skill 目录；只有经过同类
任务验证并决定长期维护的通用工具，才能另行修改 Skill。

## 一次调查并确定所有者

先确认 Python 3.11+。Survey 是对本次游戏来源的一次性调查。程序默认拒绝覆盖；
只有确认旧输出可替换时才加 `--replace`。

```powershell
python <Skill>\scripts\rpg_maker_survey.py scan --game <完整游戏安装根> --output <工作目录>\survey
```

调查后先处理未知运行消费者，即尚不知道哪个程序、插件或场景会读取该文本。必须在
所有权决定前，用隔离副本上的 `inspect_nwjs_runtime.py observe` 确认实际消费；无法访问时保持
`unresolved`，不得先分配所有者再补调查。用户禁止运行游戏、运行场景无法到达或静态证据不足时，
直接保留 `unresolved`；不要只为消除未确认项反复扫描或逐个展开大型关系组。
字符串仅仅长得像 JSON 不能证明消费者会解析它。Survey 只在活动插件源码建立了当前插件参数对象、
精确参数名到 `JSON.parse` 参数的直接数据流，或插件 schema 明确声明对应层是 `struct` / array 容器时，
展开这一层；一次 `JSON.parse` 只证明顶层一次解码，容器里的 JSON 外形字符串仍需自己的嵌套 schema
或下一次实际解析数据流。其他 JSON 外形保留完整外层字符串。已证明的序列化参数损坏时保留结构错误供
审核，不把损坏外层当玩家正文，也不让一个参数中止整个游戏调查。采用的 Rules 或外部往返方案
必须沿 Survey 记录的精确路径与解码层级写回。

`scan` 同时生成 `ownership-decisions.jsonl`：每个关系组只有一行，默认状态是
`unresolved`。把它复制到 `artifacts/decisions/ownership.jsonl`，只修改已经取得证据的组；
不要在聊天消息、补丁或临时脚本中重新生成全量决定。决定文件只引用自然 `group_id` 或
`candidate_id`：

```powershell
Copy-Item <工作目录>\survey\ownership-decisions.jsonl <任务根>\artifacts\decisions\ownership.jsonl
```

```json
{"target":"group:group-000001","owner":"rules","reason":"..."}
```

`owner` 只取 `rules`、`generic`、`exclude` 或 `unresolved`。工具已经为每组生成状态，不要求每组都能
确认所有者。拆组时删除该组的默认行，只写确实需要不同决定的成员。Generic
必须逐项写齐工具要求的七项证据；文件存在、疑似显示或源码出现引用都不够。
只有需要拆组时才导出该组的完整成员，不打开整份 `locations.jsonl`：

```powershell
python <Skill>\scripts\rpg_maker_survey.py members --survey <工作目录>\survey --group-id <group_id> --output <工作目录>\survey-members\<group_id>.jsonl
```

游戏路径和语言已固定，且 Init 的来源复制不会与当前磁盘任务争用时，可在 Agent 审核关系组时
并行执行 Init；否则按顺序执行。Extract 必须等待两者都完成。

```powershell
python <Skill>\scripts\rpg_maker_survey.py finalize --survey <工作目录>\survey --decisions <任务根>\artifacts\decisions\ownership.jsonl --output <工作目录>\plan
```

把本轮实际用于 Extract 的 `dialogue-rules.toml` 和 `rules.toml` 从计划目录复制到
`<任务根>\artifacts\rules\`；ATT 命令只引用这份已采用副本，不从聊天或临时目录取规则。

`coverage.json` 的 `complete=false` 是正常结果，表示仍有未确认范围，不是命令失败。能够取得新证据时可对
同一来源重新 finalize；证据暂时不可得时，记录精确未确认项并继续处理已确认范围，不让少量未知项卡住整次翻译。
完成的计划提供 `dialogue-rules.toml`、`rules.toml`、逐规则 manifest、Unit 投影和预期所有权。
`coverage.json` 与同目录 `rules-manifest.json` 是不可拆分的同次 finalize 证据；译后 QA 会从 Survey
和逐规则 recipe 重新投影 Unit，并拒绝由 coverage 自己声明、却没有真实 Rule 产生的所有权或 Unit。
MV 姓名 wrapper 不因外形建立全局规则；未证明的 wrapper 由译前检查按精确自然 ID 审核。

已批准 Generic 来源的 ATT 输入在 `plan/generic/input/`，精确来源映射在
`plan/generic/manifest.json`。它们只覆盖已经审核的位置。随包工具不会把 Generic 译文写回
游戏；必须使用任务中已确认的外部消费过程，并在隔离副本中验证。
建立或复核 Generic JSONL 时，能够独立理解和写回的重复记录必须分别成为 Group；不能因为
同属一个文件、数组或补丁日志就合并成一个超长 Group。ATT 只能在 Group 之间组合模型任务，
错误分组应在 Translate 前修正，不用重试或临时分片补救。

## Extract、所有权和译前准备

按发行文档 Init，并用计划中的 dialogue/Rules 执行 Extract。然后分别导出完整所有权和
本轮完整 Manual：

```powershell
att <mv或mz> ownership export --name <项目名> <工作目录>\ownership.jsonl
att <mv或mz> manual export --name <项目名> --selection all <工作目录>\final-manual.toml
python <Skill>\scripts\rpg_maker_survey.py audit --survey <工作目录>\survey --plan <工作目录>\plan --ownership <工作目录>\ownership.jsonl --output <任务根>\artifacts\qa\ownership-audit.json
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
python <Skill>\scripts\translation_preflight.py --manual <工作目录>\final-manual.toml --survey <工作目录>\survey --coverage <工作目录>\plan\coverage.json --output <工作目录>\preflight
python <Skill>\scripts\translation_preflight.py --manual <工作目录>\final-manual.toml --survey <工作目录>\survey --coverage <工作目录>\plan\coverage.json --decisions <任务根>\artifacts\decisions\placeholder.jsonl --output <工作目录>\preflight --replace
```

ATT 的 MV/MZ 内建控制符由 ATT 默认规则负责。普通未知外形、语义、术语、语言比例和布局只
进入 Review；不得把它们升级成拒绝译文的强规则。原文固定空槽和非空槽结构仍由 ATT 校验。
各 Python 助手只负责自己的调查、译前检查、术语、QA、运行观察、字体或日志输出；Agent 用显式产物
安排顺序，不增加通用流水线框架，也不让助手互相调用。

## Translate 与集中返修

术语使用 `skills/extract-game-terminology/SKILL.md`；Agent 可自行处理自然单元，也可用随包 Formic 批量生成候选。
资源文件名、资源路径、内部键和普通短语不进入术语，也不写入 `allowed_terms`。
译前检查和术语都定稿后再运行 Translate。Translate 持有项目租约（防止同一项目两个 ATT 命令同时
改状态的独占锁）；模型等待期间禁止对同一项目运行 export、Manual 或 WriteBack。只做不读写该项目的
隔离副本准备等独立工作；没有这类工作就等待，不启动并行命令试探。

按现行 Translate 规格运行后导出全部当前状态，再做一次静态 QA（不启动游戏的译后检查）：

```powershell
att <mv或mz或generic> translation export --name <项目名> <工作目录>\translations.jsonl
python <Skill>\scripts\translation_qa.py scan --translations <工作目录>\translations.jsonl --survey <工作目录>\survey --coverage <工作目录>\plan\coverage.json --terminology <任务根>\artifacts\rules\terminology.toml --output <工作目录>\qa
```

上面的 Survey 模式用于 MV/MZ，以及由该 Survey 计划产生的 Generic；后一种另传
`--generic-manifest <工作目录>\plan\generic\manifest.json`。没有 RPG Maker Survey 的独立 Generic
项目不建立或猜测 manifest，改用当前 JSONL 输入根：

```powershell
python <Skill>\scripts\translation_qa.py scan --translations <工作目录>\translations.jsonl --generic-input <Generic当前JSONL根> --terminology <任务根>\artifacts\rules\terminology.toml --output <工作目录>\qa
```

`--survey + --coverage` 与 `--generic-input` 是互斥证据；`--generic-manifest` 和
`--runtime-report` 只用于 Survey 模式。首次静态 QA 不传实际 `write_back`，因此没有静态问题时
也只能是 `unverified`；实际输出只在最终 QA 时传入。
`qa_status` 只取 `clean`、`needs_review`、
`unverified`；发现多少 Review 都不会拒绝已有结构合法译文。

先读 `qa/review-groups.jsonl` 的分类、数量和少量样例，不默认打开完整
`qa/findings.jsonl`。需要核对某组时只读取带该 `review_group_id` 的明细；确定问题由
`manual` 自动加入，启发式 Review 只有在实际审核并选中后才进入 Manual。

需要集中返修时只输出自然 ID，再让 ATT 从当前数据库预填 Manual：

```powershell
python <Skill>\scripts\translation_qa.py manual --scan <工作目录>\qa [--review-group review-000001] --output <任务根>\artifacts\decisions\revision-ids.jsonl
att <mv或mz或generic> manual export --name <项目名> --ids <任务根>\artifacts\decisions\revision-ids.jsonl <任务根>\artifacts\decisions\revision.toml
att <mv或mz或generic> manual apply --name <项目名> <任务根>\artifacts\decisions\revision.toml
```

编辑 `revision.toml` 后默认直接 apply；apply 会在单个事务内执行与 check 相同的结构校验，失败时不修改
任何条目。只在需要事先试检或单独诊断 TOML 时运行 `manual check`。通常先集中修改一轮，这是效率目标，
不是完成条件；新证据或用户实机检查发现问题时可以再做第二轮，不重复调用模型修正已经定位的 Review。

apply 成功后重新导出当前译文并再做一次静态 QA；确认本轮修改结果后才执行 WriteBack：

```powershell
att <mv或mz或generic> translation export --name <项目名> <工作目录>\translations-after-manual.jsonl
python <Skill>\scripts\translation_qa.py scan --translations <工作目录>\translations-after-manual.jsonl --survey <工作目录>\survey --coverage <工作目录>\plan\coverage.json --terminology <任务根>\artifacts\rules\terminology.toml --output <工作目录>\qa-after-manual
```

独立 Generic 在这一轮继续使用同一个 `--generic-input`，不得改回 Survey 参数或补造 manifest。

## WriteBack、最终运行观察和 QA

集中 Manual apply 后执行 WriteBack，再把输出部署到可丢弃的隔离游戏副本。先在该副本完成最终
字体 apply，再按玩家方式正常启动并等待标题或第一个可交互画面。优先使用 smoke 尝试标准场景，
需要检查正常游玩分支时再用 observe 记录实际绘制；这些操作都只在隔离副本执行：

只有已经确认具体位置和宽度时才编写 WriteBack 排版规则，并把采用的文件保存到
`<任务根>\artifacts\rules\write-back-layout.toml`。首次采用时传
`--layout-rules <该文件>`；校验成功后项目保存规范内容，后续省略参数即可复用。要明确清空时
传入只含 `rule = []` 的文件。不得用一条统一宽度规则覆盖未经确认的全部文本；字段和
401/405、字符串 LF 的物化边界以 `docs/translation/write-back-layout-rules.md` 为准。

默认直接 apply；apply 每次都按隔离副本和最终字体输入重新扫描、生成计划并验证，同时输出检查报告。
只在需要修改前比较或只读调查时先 inspect。每次修改都有事务记录，可按记录 restore：

```powershell
python <Skill>\scripts\manage_rpg_maker_fonts.py apply --game <隔离副本> --font noto-sans-sc --translations <工作目录>\translations-after-manual.jsonl --state <工作目录>\font-state --output <任务根>\artifacts\qa\font-apply.json
python <Skill>\scripts\manage_rpg_maker_fonts.py restore --game <隔离副本> --state <工作目录>\font-state --output <任务根>\artifacts\qa\font-restore.json
```

字体工具递归处理已确认的完整字体引用，不只修改 MV `gamefont.css` 或 MZ 的单个标准字段；
`@font-face` 和字体加载 API 已注册的运行时别名必须保留，只替换别名指向的字体资源。
项目字符投影只读取严格校验后的当前 ATT `translation export`，并按 WriteBack 的 current 译文或
未接受条目的原文计算；这份导出没有同时绑定 Survey、finalize coverage、实际 WriteBack 和运行
副本，不能证明项目全部字体消费者范围。`--coverage-text` 只用于另有真实消费者的补充玩家文本。
因此字体工具没有发现问题时仍报告 scoped `unverified`；缺字属于 `needs_review`，并必须检查
`review_required`，不能把“静态引用 Review 为空”误读成字体覆盖通过。

```powershell
python <Skill>\scripts\inspect_nwjs_runtime.py smoke --game <隔离副本> --output <任务根>\artifacts\qa\nwjs-smoke --confirm-isolated-copy
python <Skill>\scripts\inspect_nwjs_runtime.py observe --game <隔离副本> --output <任务根>\artifacts\qa\nwjs-observe --confirm-isolated-copy
```

smoke 必须等游戏离开启动场景，并把未捕获异常、引擎错误画面和启动超时记为 `needs_review`。
场景只有在观察器必需 hooks 完整并完成安装轮询、动作明确受支持、本场景原子序列边界内存在
语义匹配的非空文本绘制，且截图是至少 64×64、可完整解码的 PNG 时才能标为 `verified`；不能沿用
启动期、切换期或前序场景累计的绘制。单个后续场景无法安全进入时记为 `unverified` 并继续；未访问
场景也是 `unverified`。窗口宽度、
溢出、字体回退和英文命中只生成 Review。

完成 WriteBack、字体和 NW.js 观察后，使用 Manual 后已经重新导出的 `translation export`、同次
finalize 的 coverage、ATT 当前项目的实际 `write_back` 目录和一次完整 NW.js 观察证据目录中的
`report.json` 合并最终 QA。QA 会核对 Survey、coverage、导出自然 ID 与原文、完整 hooks/轮询、
事件字段与精确子集、场景序列边界与绘制语义、截图尺寸、写回文件，以及隔离副本中 Survey 基线
与 WriteBack 的部署字节；手写 PID、布尔值、空 hooks、空事件或 1×1 图片都不能形成通过证据。
这些一致性检查不构成无法取得的报告来源证明；执行者仍必须实际运行随包工具并保留完整目录：

```powershell
python <Skill>\scripts\translation_qa.py scan --translations <工作目录>\translations-after-manual.jsonl --survey <工作目录>\survey --coverage <工作目录>\plan\coverage.json --write-back <ATT项目目录>\write_back --runtime-report <任务根>\artifacts\qa\nwjs-smoke\report.json --terminology <任务根>\artifacts\rules\terminology.toml --output <任务根>\artifacts\qa\final
```

现行 `translation export` 不携带项目目标语言，工具不能自行证明所传导出确属本轮语言方向；即使
其余材料一致也保留 `translation_language_pair_unbound`，由任务清单中的项目身份、实际 Translate
运行和语言验收另行闭合，不能删掉该未验证项来换取 clean。
RPG Maker 的现有 Survey/coverage 也没有完整 WriteBack recipe，工具能够核对输出树、JSON、应有文件
及隔离副本部署字节，并只用同一输出文件中源文的精确文字序列提出残留 Review；这种文件级命中
不能证明每个自然 ID 的精确写回位置，因此保留
`rpg_write_back_unit_mapping_unverified`；按验收指南检查真实差异和场景，不能把该项改写成已验证。

Survey 计划产生的 Generic 另传 `--generic-manifest` 和实际 Generic `write_back`。当前没有通用的
外部反向转换或游戏消费者报告格式，因此 Generic 最终 QA 保持 `unverified`，直到任务按已确认的
外部消费过程另行取得并记录实际结果；不要为让工具显示 clean 手造报告。

独立 Generic 的最终 QA 使用当前输入和实际输出，不传 Survey 专用参数：

```powershell
python <Skill>\scripts\translation_qa.py scan --translations <工作目录>\translations-after-manual.jsonl --generic-input <Generic当前JSONL根> --write-back <ATT项目目录>\write_back --terminology <任务根>\artifacts\rules\terminology.toml --output <任务根>\artifacts\qa\final
```

该模式会核对 JSONL 文件集合、完整自然 ID 顺序、Group/Unit 身份、Current 译文和保留原文；
外部来源映射、反向转换与实际消费者仍分别保持 `unverified`，必须沿任务已经确认的外部流程验收。

运行报告、用户实机检查或返修后新出现的可观察事实可以触发下一轮 Manual、WriteBack 和受影响场景复查；
不得为了避免第二轮而在译前穷举无法确认的内容，也不得因同一静态问题重复返工。

部分 NW.js 包装器不接受 smoke 使用的调试端口。此时不能跳过启动：改用 Windows 界面按玩家方式
启动同一隔离副本，等待正常交互画面，确认没有错误并保存截图。两种方式都无法取得正常启动证据，
或实际出现运行时错误时，产物只能称为静态候选，处理完成前不得交给用户作为首次可测试译本。
译本启动失败后，再用未修改的原版隔离副本执行相同检查以判断问题来源；原版通过而译本失败时，
即使最先报错的插件文件没有变化，也必须先检查翻译、字体、资源或配置是否破坏了它依赖的启动条件。

## 恢复与完成

- Formic 中断时保留同一 input、plan、task、OUT 和 `results`，修正原因后用 `--resume`。
- Python 输入损坏、来源变化或决定冲突时按 stderr 修正；Review 和未验证项保持退出 0。
- 精确来源仍缺运行消费者证据时，使用 `inspect_nwjs_runtime.py observe` 记录实际消费；
  无法访问的场景保持 `unverified`。
- 完成必须覆盖声明范围、所有项目、全部输出、Generic 外部消费和实际场景；一次成功退出、
  `Complete` 或抽样检查都不能单独证明整个游戏完成。
- 只有取得正常启动证据后，首次交付才表示用户已有可测试版本；用户实玩反馈尚未处理时，任务处于等待反馈状态，
  不能声称循环结束。反馈到达后只复查受影响范围，不重复已经验证且未受影响的首次流程。
