# ATT 生产运行时与 CLI 现行规格

## 1. 统一入口与全局界面选项

RPG Maker 是 ATT 当前唯一已实现的游戏领域，该领域支持 MV 与 MZ。CLI 使用
`mv | mz` 选择域内目录布局和工作区身份；两者共享业务实现，但拥有彼此独立的项目、
租约和运行方案。

```text
att --config FILE [--ui-language LANG] [--progress auto|plain|off] mz COMMAND ...
att --config FILE [--ui-language LANG] [--progress auto|plain|off] mv COMMAND ...
```

除 Help 和 Version 外，`--config FILE`、`mv | mz`、子命令和 `--name NAME` 始终需要
显式提供。不存在默认配置路径。`--progress` 默认是 `auto`；UI 语言也可以通过
`ATT_UI_LANGUAGE` 提供。UI 语言和进度模式不写入项目数据库。

当前命令形状为：

```text
att mz|mv init --name NAME [--path GAME_ROOT]
  [--source-language LANG]
  [--target-language LANG]
  [--dialogue-max-fullwidth-chars COUNT]
  [--scrolling-text-max-fullwidth-chars COUNT]
  [--help-description-max-fullwidth-chars COUNT]

att mz extract --name NAME
  [--builtin] [--rules RULES_TOML] [--lua SCRIPT_LUA]

att mv extract --name NAME
  [--builtin] [--rules RULES_TOML] [--lua SCRIPT_LUA]
  [--dialogue-rules DIALOGUE_TOML]

att mz|mv translate --name NAME [PROFILE_ID]
  [--terms TERMS_TOML]
  [--placeholders PLACEHOLDERS_TOML]
  [--lua SCRIPT_LUA]

att mz|mv write-back --name NAME [--lua SCRIPT_LUA]

att mz|mv lua --name NAME [--profile PROFILE_ID] SCRIPT_LUA [-- ARG...]
```

省略可选参数不等于使用模糊的“默认值”。命令会把每项选择解析成类型化来源：本次显式
输入、项目保存状态或固定产品行为，并在终端摘要和项目日志中如实说明来源。一次命令中
不同字段可以来自不同来源；例如显式 Translate Profile 可以同时复用项目保存的 Lua。

## 2. UI 语言

终端 Help、Usage、CLI 解析错误、配置诊断、业务提示、进度、最终结果和项目日志消息
使用同一个闭集 UI locale。支持：

```text
ar  zh-Hans  zh-Hant  en  fr  ru  es  ja  ko  vi
```

选择优先级固定为：

```text
--ui-language
  > ATT_UI_LANGUAGE
  > Windows 用户首选 UI 语言的有序列表
  > en
```

显式来源中的非法或不支持 locale 是输入错误；Windows 自动检测中的无效或不支持项会
跳过，全部不能匹配时回退英语。匹配规则如下：

- `zh-Hant`、`zh-TW`、`zh-HK`、`zh-MO` 选择 `zh-Hant`；
- `zh-Hans`、`zh-CN`、`zh-SG` 和普通 `zh` 选择 `zh-Hans`；
- 其他受支持语言的区域变体按主语言匹配，例如 `fr-CA` 选择 `fr`。

这个选择在进程入口只解析一次。Translate 配置使用 `prompts.locale = "auto"` 时直接
复用该有效 UI locale 选择外置 Prompt，不重新读取参数、环境或 Windows 设置；显式
`prompts.locale` 则覆盖它。两者都不改变项目 metadata 中的源语言或目标语言。

用户控制的路径、ID 和其他动态文本在进入终端或日志前会移除 ESC、换行伪装及既有
双向文本控制字符。阿拉伯语消息保持逻辑顺序；命令、路径、ID、数字和进度条作为独立
方向片段渲染，不反转原字符串。

## 3. 运行方案解析与持久化

项目数据库为 Init、Extract、Translate、WriteBack 四个阶段命令分别保存上次成功运行
方案。项目租约覆盖方案读取、业务执行、必要收尾和最终方案替换，防止两个并发命令把
不同选择拼成同一方案。独立 `lua` 命令也持有同一项目租约，但不读取或写入自己的运行
方案。

### 3.1 Init

首次 Init 必须提供 `--path`、源语言、目标语言和三类宽度。已有项目可以省略 `--path`，
此时复用上次成功 Init 保存的来源路径；语言与宽度分别复用项目 metadata 中的当前值。
显式提供的路径或设置只替换对应项。首次运行缺少必需事实，或已有项目没有可复用来源
路径时，返回包含下一步操作的输入错误。

### 3.2 Extract

未提供 `--builtin`、`--rules`、`--lua` 中的任何一项时，复用上次成功 Extract 保存的
完整 owner 集合，并提示例如“未提供提取范围，已沿用上次成功方案：Builtin、Rules”。
项目尚无保存方案时省略全部 owner 是输入错误。

只要显式提供任一 owner，本次显式集合就精确替换旧集合：未列出的 owner 本次不执行，
也不会仅因未列出而删除其既有资产。以下输入承担明确的清除语义：

- Rules 文件的 canonical 内容为 `rule = []` 时，停用 Rules owner，并从以后自动复用的
  Extract 方案中移除 Rules；
- 零字节 Extract Lua 文件不执行程序，停用 Lua owner、删除该 owner 的标准资产，并清除
  Extract 阶段 Lua 程序；
- 清除后没有任何可执行 owner 时删除保存的 Extract 方案；本次清除可以成功，但下一次
  无参数 Extract 会报告“尚无可复用方案”。

非空 Rules 输入在边界完成校验，项目只保存其 canonical 语义；以后复用不再读取原 TOML
路径。MV 的 `--dialogue-rules` 仍只允许与 Builtin 同时使用：省略时复用项目 definition，
canonical `rule = []` 清空该 definition。它不是独立 owner。

### 3.3 Translate

`PROFILE_ID` 可以省略；省略时精确复用上次成功 Translate 保存的 Profile。保存的 ID 在
当前配置中不存在时显式失败，不按语言、顺序或其他 Profile 回退。显式 Profile 只替换
本次 Profile 选择。

术语和 Placeholder 继续以项目 canonical 资源为权威：省略文件时复用项目状态，术语的
`term = []` 和 Placeholder 的 `rule = []` 分别清空对应资源。非空 `--lua` 替换并执行
Translate 阶段程序；省略时自动复用该阶段保存的程序；零字节文件清除该程序且本次不执行
Lua。清除不会猜测或删除 Lua 私有数据库状态。

Translate 还会严格消费 `[prompts]` 的 `root`、`locale` 和 `thinking_output`，每次运行
重新读取规范 locale 下的 `system.md`；只有开启思考输出时才读取同目录 `thinking.md`。
所选资源或模板无效时在任何 LLM 请求前失败。关闭模式要求 JSON-only 响应，开启模式
要求精确的一组非空 `<why>...</why>` 后接同一 JSON wire；信封或 JSON 解析失败形成
`ModelResponseUnusable`，不会作为网络错误重试。

Translate 同时消费 `[rpg_maker]` 中可省略的 `record_translation_tasks`，默认 `false`。
启用时，每个已启动的 Standard TaskBlock 在同一 RunId 下生成一份人类可读任务记录，
同一任务的全部重试、输入输出、验收和提交终态归入该文件。Translate Lua 不生成任务
记录；该开关不进入翻译 state、保存方案或项目数据库。

### 3.4 WriteBack

项目从未保存 WriteBack Lua 选择时，省略 `--lua` 使用固定产品行为 Standard-only。
以后省略时复用上次成功选择。非空 Lua 文件替换并启用 WriteBack 阶段程序；零字节文件
清除程序，本轮只执行 Standard，并且不处理 Lua 私有数据库状态。

### 3.5 独立项目 Lua

`lua` 是一次性项目程序入口。`SCRIPT_LUA` 每次必填，ATT 从本次解析后的路径重新读取
完整文件；零字节文件是合法空程序，主 chunk 返回值忽略。脚本、参数和 Profile 选择都
不写入任何阶段运行方案，也不改变 Extract、Translate 或 WriteBack 已保存的 Lua 快照。
`--` 后的参数保持顺序放入
全局 `arg[1..]`，`arg[0]` 是解析后的脚本路径；任一参数不能表示为 UTF-8 时在运行脚本前
明确失败。

独立程序拥有完整可信 Lua 5.4 和公共项目接口。它可通过 `ctx.standard.open()` 打开由
Standard 核心拥有的人工候选会话，但不会发送 LLM 请求：

- 显式 `--profile` 在当前配置中精确选择，ID 不存在时失败；
- 未显式选择时，`open()` 才读取上次成功 Translate 保存的 Profile；项目没有可复用 ID，
  或保存 ID 已不在当前配置中时，只让 `open()` 失败，不妨碍不使用 Standard 的普通脚本；
- 显式或复用的 Profile 只服务本次会话，不替换 Translate 保存方案；
- 术语和 Placeholder 始终读取项目当前 canonical 资源，不接受本命令临时覆盖。

一次成功的 `ctx.standard.accept` 已经完成独立短事务提交。脚本之后失败或取消不会回滚
更早已经确认的调用；原游戏目录不被该接口修改。可信脚本通过标准库或 `ctx.db` 自行产生
的其他副作用仍由脚本作者负责。

### 3.6 Lua 快照

Extract、Translate、WriteBack 的非空 Lua 主程序按阶段分别保存在项目数据库中，包括
程序正文 BLOB、SHA-256 和无损 Windows 规范解析路径。自动复用执行保存的正文，不重新
读取原文件；原路径只用于 chunk 名、`require` 搜索目录和诊断。因此主文件移动或被修改
不会改变保存方案。脚本主动加载的模块、文件和进程仍是可信 Lua 的外部动态依赖，不
纳入快照。

独立 `lua` 命令不属于本节快照：它没有复用、清除或保存语义。

### 3.7 成功替换边界

运行方案不是尽力而为的日志，而是后续命令会消费的项目状态。只有业务成功且所有必要
非日志根完成收尾后，ATT 才在最后一个短 SQLite 事务中原子替换本命令的整套方案：

- 业务失败、取消或非日志收尾失败：不尝试替换；
- 事务确认回滚：旧方案保持不变，退出 `1`；
- 提交终态无法确认：退出 `1`，说明业务结果与方案状态不能确认，并建议下次显式传参；
- 业务副作用已经生效但方案保存失败：明确报告“结果已生效、运行方案未保存”，不伪装
  成普通成功。

## 4. 启动与命令构造

一次普通运行按以下职责顺序收敛：

```text
解析 UI locale、进度模式、CLI 意图和必填 --config
  ↓
完整读取一次 TOML，检查 UTF-8、完整语法与未知顶层分区
  ↓
建立通用受信配置
  ↓
取得对应版本的项目租约，读取项目事实和保存方案
  ↓ 工作区合法建立后
启动本 RunId 独占的项目 JSONL（失败时明确警告并降级）
  ↓
把显式输入、项目状态或产品行为解析为本次完整方案
  ↓
只构造本次命令消费的 Profile、Client、Lua 和纵向根能力
  ↓
执行命令，完成必要非日志收尾；阶段命令原子保存运行方案
  ↓
呈现最终结果；项目日志独立尝试排空，不参与业务终态
```

Translate 在选定显式或保存的 Profile 后，精确选择当前配置中的 Profile 及其 Client；
项目开启后取得 metadata 的规范 `LanguagePair`，再按 `[prompts].locale` 选择
`<prompts.root>/rpg_maker/<locale>/system.md`，用两个项目 `LanguageId` 渲染模板。仅当
`thinking_output = true` 时读取并追加同 locale 的 `thinking.md`。Prompt 只按所选 locale
的精确路径读取。只有最终方案启用某阶段 Lua 时才构造程序固定策略的 Lua Runtime；配置
不包含 Lua Runtime 分区。

独立 `lua` 每次从显式路径构造程序。未显式提供 Profile 时，公共 Lua 能力先运行；
`ctx.standard.open()` 才解析保存的 Translate Profile 并装配 Standard 语义。该入口不构造
LLM 请求执行器，不建立虚假的 Standard TaskBlock。

Extract、Translate、WriteBack 与独立 Lua 各自在命令生命周期内构造一个私有 Rayon CPU
池。文档解析、规则扫描、资产编解码、规划准备、人工 Standard 准备与写回计算共享操作
系统可用并行度。饱和只会自然背压；等待时取消则任务不执行，已经准入的任务会完成，
shutdown 停止新准入并排空已接管作业。

## 5. 进度与输出通道

进度只呈现业务已经确认的绝对事实，不制造全局百分比或 ETA。每个纵向切片拥有自己的
阶段枚举，共享渲染只接收 `Indeterminate` 或 `{completed, total}` 快照：

- Init：检查项目、扫描来源、构建候选、数据库收敛、发布、保存运行方案使用阶段 spinner；
- Extract：先显示 owner 阶段 `i/N`；文档、Builtin 工作单元和 Rules 规则在真实分母建立
  后显示局部进度，Lua 与 SQLite 提交使用 spinner；
- Translate：规划完成后显示“已确认任务 x/N”，只在该任务所需数据库提交成功后推进；
  Complete、Partial、Unavailable 都计入，零任务提示“无需调用模型”且不显示 `0/0`；
- WriteBack：资产读取、规划和文档改写使用可取得的真实计数；Lua、候选验证和发布使用
  spinner；
- Lua：程序执行和每次 Standard 人工提交使用 spinner，不制造 LLM 任务进度；
- 达到 `N/N` 后进入“正在收尾/保存运行方案”，所有必要业务操作完成后才显示成功；
- Ctrl-C 立即显示“正在安全停止”，并保留最后一个已确认计数。

三种渲染模式为：

| 模式 | 行为 |
|---|---|
| `auto` | stderr 是 TTY 时以约 10 Hz 更新单行 ASCII spinner/进度条；非 TTY 完全不输出实时状态 |
| `plain` | 在 stderr 输出稀疏阶段行和关键计数，不使用 ANSI、回车覆盖或逐任务刷新 |
| `off` | 关闭实时状态和进度，不关闭最终结果、错误、非阻断日志健康警告或项目日志 |

颜色不承载语义；即使启用，也只在 TTY 且未设置 `NO_COLOR` 时使用。稳定最终摘要写 stdout，
实时状态、进度和非阻断提示写 stderr；错误也写 stderr。最终摘要包含关键方案来源、实际
结果和必要的下一步建议。日志只写日志文件，不混入 stdout。

## 6. 项目布局、租约与取消

MZ 只接受顶层同时包含 `data/`、`js/` 和 `js/rmmz_core.js` 的游戏根；MV 只接受包含
`www/data`、`www/js` 和 `www/js/rpg_core.js` 的游戏根。不探测另一种布局，不自动修正
传入的 MV `www`。

```text
<projects.root>/<engine>/<project-name>
<projects.root>/.att-locks/projects/<engine>/<digest>.lock
<projects.root>/.att-locks/directory-publish/<engine>/
```

同一引擎版本、同一项目的五个命令互斥；不同引擎版本的同名项目独立。锁顺序固定为
“项目租约 → 目录发布锁 → SQLite/session”。等待项目租约、目录发布锁或 SQLite busy
不设置任意截止时间；等待过程响应 Ctrl-C/shutdown，取消后不开始后续副作用。

第一次 Ctrl-C 后停止派生新阶段；SQLite、发布、CPU 和 HTTP 已接管的工作继续到明确终态。
候选尚未发布时 discard；publish 已开始时等待终态。运行方案不在取消路径保存。普通项目
日志故障不会改变取消与收尾次序。Translate 停止启动新的 Standard 任务；每个已发出
`TaskStarted` 的任务仍返回顺序最终化边界，并在文件系统 shutdown 前形成已提交、未提交
或已取消的明确任务记录终态。没有启动的任务不生成记录。

## 7. 日志、退出码与安全诊断

项目日志是可降级的可观测性旁路，不是网络请求、数据库提交、目录发布、恢复、取消或
成功退出的门禁。工作区合法建立后，每个 RunId 独占一个 JSONL 文件；日志不共享活动
文件、不轮转，也不按大小丢弃事实。建立、写入、flush、sync 或关闭失败时，stderr 必须
显示日志路径、具体操作和清理后的底层原因，但不改变原本的成功、失败或取消退出码。
详细契约见[普通项目日志](project-log.md)。

启用 `rpg_maker.record_translation_tasks` 时，Standard 任务记录写入
`task-records/<run-id>/task-000001.md`。它与普通 JSONL 一样是可降级、非权威的
可观测性旁路，但提供单任务的完整可读上下文；缺失记录不证明没有调用，记录故障也不
改变原业务结果、项目状态、后续任务或退出码。完整契约见
[Standard 翻译任务记录现行规格](../rpg-maker/task-records.md)。

项目日志建立后的命令 panic 由命令边界转换成 `internal.operation` 安全诊断：CLI 与
JSONL 都显示实际命令阶段、项目工作区、日志路径和 `outcome_unknown` 影响，绝不显示
panic payload；JSONL 以未知终态完成，CLI 退出 `1`。只有日志建立前或日志无法建立时的
panic 才由最外层进程兜底直接写 stderr。

| 退出码 | 含义 |
|---|---|
| `0` | Help、Version 或命令成功，包括正常 Partial/Unavailable |
| `2` | CLI 解析错误 |
| `1` | 配置、输入、业务或技术错误；非日志 shutdown 失败；运行方案保存失败或终态未知 |
| `130` | Ctrl-C 后完成受控收尾；日志故障仍保持 `130` |

错误统一说明错误码、阶段、对象或路径、具体原因、稳定底层代码、状态影响、处理办法和
恢复位置。OS 系统消息、SQLite primary/extended code、HTTP 状态与允许公开的供应商
code/type 在清理控制字符后明示；不得用责任域类别替代具体原因。输出不读取任意 `Debug`
或内部来源链，也不泄露 API key 实际值。CLI 与普通 JSONL 不复制配置原文、Header、
完整 Client parameters、Prompt、完整模型消息、模型正文、原文或译文，是为了维持职责、
稳定 schema、可读体积和控制字符边界，不表示这些内容属于敏感信息。任务记录可以呈现
上述任务正文，并对其中出现的 API key 实际值作精确替换。
