# ATT CLI 现行规格

## 1. 统一入口

```text
att [--ui-language LANG] ENGINE COMMAND ...
```

`ENGINE` 取 `mv`、`mz` 或 `generic`。Help 与 Version 开箱即用；其余命令读取实际运行的
`att.exe` 同目录下固定 `config.toml`，并要求显式给出引擎、命令和项目名。CLI 不提供配置
路径参数。

当前命令：

```text
att mv|mz init --name NAME [--path GAME_ROOT]
  [--source-language LANG] [--target-language LANG]

att generic init --name NAME [--path JSONL_ROOT]
  [--source-language LANG] [--target-language LANG]

att mz extract --name NAME [--builtin] [--rules RULES_TOML]
att mv extract --name NAME [--builtin] [--rules RULES_TOML]
  [--dialogue-rules DIALOGUE_TOML]
att generic extract --name NAME

att mv|mz|generic translate --name NAME [PROFILE_ID]
  [--terms TERMS_TOML] [--placeholders PLACEHOLDERS_TOML]
  [--retry-rejected]

att mv|mz ownership export --name NAME OWNERSHIP.jsonl
att mv|mz|generic translation export --name NAME TRANSLATIONS.jsonl

att mv|mz|generic manual export --name NAME
  [--selection pending|rejected|all | --ids IDS.jsonl] FILE.toml
att mv|mz|generic manual check  --name NAME FILE.toml
att mv|mz|generic manual apply  --name NAME FILE.toml

att mv|mz|generic write-back --name NAME [--layout-rules LAYOUT_RULES_TOML]

att mv|mz|generic lua --name NAME SCRIPT.lua [-- ARG...]
```

Extract、Translate 和 WriteBack 不接受 `--lua`。Manual 不接受兼容格式或模型参数；
`manual export` 省略筛选时使用 `pending`，`--selection` 与 `--ids` 互斥。独立 Lua 不接受
`--profile`。

## 2. UI 与进度

实时进度没有 CLI 或配置选项。ATT 在交互终端、管道、重定向和测试 Harness 中都向 stderr
打印普通 UTF-8 行，不使用 spinner、进度条、回车覆盖或 ANSI 控制。尚无真实总量时，每个
阶段只打印一次阶段名；总量为零时打印该阶段无需处理。已有总量时，首个快照必定打印，之后
只在整数百分比变化时打印：

```text
已确认翻译任务：0%（0/8354）
已确认翻译任务：37%（3090/8354）
已确认翻译任务：100%（8354/8354）
```

百分比按已经确认的 `已完成/总量` 向下取整。快照从 33% 跳到 36% 时只打印实际观察到的
36%，不补造中间百分比。失败或取消不会补写 100%；安全停止和必要收尾按真实发生顺序各写
一行。每阶段最多输出 101 条百分比行。

UI 语言可由 `--ui-language` 或 `ATT_UI_LANGUAGE` 指定，选择优先级为：

```text
--ui-language > ATT_UI_LANGUAGE > Windows 用户首选 UI 语言 > en
```

支持：

```text
ar  zh-Hans  zh-Hant  en  fr  ru  es  ja  ko  vi
```

区域变体按主语言归并，中文按简繁区域区分。显式非法值按输入错误处理；自动检测匹配不上
时回退英语。Prompt 固定使用中文指令并根据项目语言对渲染源语言和目标语言，不跟随 UI
语言变化。

路径、可读 ID 和动态文本进入终端与日志前，会先移除终端转义、换行伪装和双向控制字符。
Windows 路径统一显示自然盘符或 UNC 形式，不公开 `\\?\` 扩展路径前缀。
普通 CLI 不显示 hash、UUID、数据库随机键、编码位置或供应商请求 ID。

## 3. 保存状态与省略参数

省略可选参数时，ATT 只在以下明确说明的情形复用项目状态：

- Generic 首次 Init 必须提供路径和语言；再次 Init 分项复用；
- MV/MZ 首次 Init 必须提供游戏路径和语言；再次 Init 分项复用；
- MV/MZ 首次 Extract 必须选择 owner；以后省略全部选项时复用完整 owner 集合；
- Translate 省略 Profile 时复用最近成功 Profile；省略术语或 Placeholder 时复用项目资源；
- Generic Extract 始终读取项目绑定的当前 JSONL 根；
- Manual 每次读取显式 TOML；
- WriteBack 提供 `--layout-rules FILE` 时，完整校验并保存规则内容后用于本次写回；省略时
  复用项目已经保存的规则；`rule = []` 清空规则；
- Lua 每次读取显式脚本。

排版规则保存的是规范内容而不是文件路径。新文件无效时本次 WriteBack 失败，旧规则保持；
尚未保存规则的项目省略参数时不自动排版。完整格式见
[WriteBack 排版规则规格](../translation/write-back-layout-rules.md)。

从状态读取、业务执行、必要收尾到最终保存，项目租约全程在场，两条并发命令不能拼出一份
不存在的选择。

Profile、Client、Prompt、术语和语言检查阈值是后续 Translate 请求的选择，不是已有正文
的所有权或适用性条件。更换这些选择不会使既有译文失去 Current。项目语言对不属于
这类请求选择；它改变后，旧语言对正文保留但不发布。

## 4. 启动、取消与资源

程序只按命令的实际需要建立配置、项目、Prompt、Client、文件系统、SQLite 和 Lua 能力。
Manual、Lua、Init、Extract 与 WriteBack 不构造模型 Client；Manual check/apply 和 Lua 高级
译文 API 也不会请求模型。

模型 Task 只在第一次真实外部 HTTP attempt 开始时计为 started。请求构造失败、准入前
取消或服务停发门拒绝都不得伪造 started、attempt 或任务记录。

文件解析与相互独立的工作默认并行；要求确定顺序的结果仍按自然顺序合并和提交。处理窗口
装满时上游等待，不把合法项目总量变成容量错误。

Ctrl-C 请求合作取消：

- 不再开始新的模型请求或文件任务；
- 等待已经进入提交或目录交换边界的操作得到明确结果；
- 保存已经确认的 Translate 前序进度；
- 完成已经形成请求、响应或明确终态的模型任务记录；
- Lua 只回滚取消时仍打开的事务，之前的 autocommit 或显式 COMMIT 保留；
- 以退出码 `130` 结束。

## 5. Manual 输出

Manual 的完整格式和检查规则见 [Manual TOML 规格](../manual/README.md)。CLI 摘要只报告：

- export：导出条目数和目标文件；
- check：有效、未填写和错误数量；
- apply：已应用、未填写和错误数量。

检查错误逐项显示可读 `id`、原因和修改方法。未填写项不是错误，因此 check 在只有未填写项
时仍退出 `0`。apply 发现任一错误时不修改数据库并退出 `1`。

`ownership export` 导出全部 RPG Maker Extract Unit 的自然所有权；`translation export`
导出全部当前 Unit 的原文、译文和状态。两者都从一个只读快照原子写出 JSONL，不依附
Manual 筛选。

Ownership 每行只含自然 ID、owner，以及 Rules 条目的自然规则序号：

```json
{"manual_id":"Actors.json:1:name","owner":"builtin"}
{"manual_id":"plugins.js:QuestWindow:Title","owner":"rules","rule_number":7}
```

Translation 每行含 `manual_id`、`source`、`translation`、`state`、`origin` 和 `type`；
RPG Maker 另含 owner 和 Rules 的 `rule_number`。`state` 为 `pending`、`current` 或
`rejected`，`origin` 为 `none`、`automatic` 或 `manual`。只有 current 的 `translation` 是
已经接受的 string array；pending 和 rejected 都是 `null`。Rejected 另含
`rejected_candidate_json` string，逐字保存原候选 JSON 文本；该文本是诊断材料，不作为外层
JSONL 的嵌套 JSON 解析。输出始终一行一条完整 JSON，不含数据库 ID、hash 或编码位置。

`current` 表示译文对当前源文、实际 Group 语境、项目语言对和强不变量仍可发布；它不表示
译文由当前 Client、Profile 或 Prompt 生成。保留但不适用于当前事实的正文不作为
`current` 导出或发布。

## 6. 运行文件

Init、Extract、Translate、WriteBack 和 Lua 建立自然序号 RunId；Manual 只输出自己的检查摘要，
不建立项目日志。RunId 形式为：

```text
run-000001
run-000002
```

同次运行的文件使用相同 RunId：

```text
<project>/logs/run-000001.jsonl
<project>/task-records/run-000001/task-000001.md
```

ATT 在项目租约内扫描既有日志和任务记录，并用原子创建保留下一个编号。冲突时递增，不用
UUID、hash 或额外计数数据库。

## 7. 输出与退出码

stdout 呈现最终业务结果，实时进度、警告、降级和错误走 stderr。每个建立项目日志的命令
还显示一次 `运行记录：<自然日志路径>`：成功写 stdout，失败或取消写 stderr。错误和警告
统一说明对象、直接原因、状态影响和处理办法，不倾倒内部阶段、状态字段袋、数据库行、
SQLite code、查询文本、供应商请求 ID 或指纹差异。

```text
错误：
对象：……
原因：……
影响：……
处理办法：……
```

警告使用同样四项，只把首行改为 `警告：`。相关失败在同一主块之后使用本地化的关系标题，
不重复显示第二个主错误标题。

项目日志仍可写时，警告和错误以同样的可读四字段保存；日志无法建立或继续写入时，stderr
直接显示这四项。相关清理、回滚、候选丢弃、收尾、关闭和结果记录失败还要说明彼此关系。
Partial、Unavailable、Review、取消后已经生效的状态和任务记录故障都必须可见，但不重复
输出无实际作用的确认或内部诊断。

Translate 的正常终态明确显示 `无需处理`、`完整` 或 `未完整`。Partial 或 Unavailable
属于结果明确的未完整状态，继续退出 `0`；stderr 同时用一条汇总警告说明任务、协议、请求
耗尽、冲突和剩余数量，逐任务详情保留在同次项目日志与任务记录中。

Translate 失败或取消时，如果此前已经形成计划和引擎汇总，stderr 在错误或取消文案附近
打印一次同源短汇总：planned、started、not_started、失败或取消 Task、剩余工作，以及请求
准入是否已经停止。该汇总与项目 JSONL 使用同一份终态事实；不会逐 Task 刷新终端。规划前
失败或提前取消不伪造引擎工作量，只保留零 Task 事实和主诊断。永久认证、授权、额度或账户错误使 Translate
失败并退出 `1`。普通 429 等待过长或重试耗尽时，当前 Task 为 Unavailable，后续 Task 为
not_started，Translate 显示未完整并退出 `0`；普通网络或 500 耗尽不会停止后续 Task。

项目日志或任务记录故障本身不改变已经确定的业务结果和项目状态。用于呈现警告、错误、
成功结果或取消终态的 stdout/stderr 写入、flush、后台线程或 channel 失败时，进程不能
假装已经告知使用者，必须返回 `1`。

stdout 或 stderr 的首次写入、flush 失败时，ATT 保留自上次成功 flush 以来尚未确认的完整
正文，在项目日志关闭前记录该呈现失败，然后把正文和对应四字段诊断合成一个批次，只向
仍可用的另一条流写入并 flush 一次。已经失败的流不重试；相反流的这次有界回退再失败时
直接返回 `1`，不递归建立新诊断、不重新关闭项目日志，也不反复输出同一正文。

- `0`：命令得到明确成功结果，包括已明确的 Partial、Unavailable，以及只有未填写项的
  `manual check`；
- `1`：输入、检查、运行、提交、发布或呈现失败；
- `130`：受控取消。

状态已经明确但必须保留或处理恢复现场时，ATT 显示实际对象、原因、影响、处理办法和自然恢复
路径。只有提交、发布或进程异常使最终状态确实无法确认时，才报告结果未知；两者都不能
伪造成成功或已经回滚。
