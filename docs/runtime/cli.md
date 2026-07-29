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

省略可选参数不等于使用模糊的“默认值”。命令会明确记录每项选择的来源：本次显式
输入、项目保存状态或固定产品行为，并在终端摘要和项目日志中如实说明来源。一次命令中
不同字段可以来自不同来源；例如显式 Translate Profile 可以同时复用项目保存的 Lua。

## 2. UI 语言

终端 Help、Usage、CLI 解析错误、配置诊断、业务提示、进度、最终结果和项目日志消息
使用同一个 UI locale。支持的值固定为：

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
跳过，全部不能匹配时回退英语。`--ui-language` 预扫描只为 Clap 选择呈现语言；预扫描
本身无法得到有效值时仍执行 Clap 的完整参数检查，并优先报告更早出现的实际参数错误。
匹配规则如下：

- `zh-Hant`、`zh-TW`、`zh-HK`、`zh-MO` 选择 `zh-Hant`；
- `zh-Hans`、`zh-CN`、`zh-SG` 和普通 `zh` 选择 `zh-Hans`；
- 其他受支持语言的区域变体按主语言匹配，例如 `fr-CA` 选择 `fr`。

这个选择在进程入口只解析一次。Translate 配置使用 `prompts.locale = "auto"` 时直接
复用该有效 UI locale 选择外置 Prompt，不重新读取参数、环境或 Windows 设置；显式
`prompts.locale` 则覆盖它。两者都不改变项目 metadata 中的源语言或目标语言。

用户控制的路径、ID 和其他动态文本在进入终端或日志前会移除 ESC、换行伪装及既有
双向文本控制字符。阿拉伯语消息保持逻辑顺序；命令、路径、ID、数字和进度条作为独立
方向片段渲染，不反转原字符串。

安全诊断中的 stage、impact、action、failure、I/O 类别、配置规则和 TOML 值形态都来自
固定列表，十个 locale 分别通过 Fluent 提供完整译文，不以英语说明或枚举 Debug 文本
回退。HTTP、SQLite、Windows 与 OS 错误的可选事实只在实际存在时按字段组合；呈现结果
不会出现 `Some(...)`、`None` 或 Debug 引号。由稳定 OS 错误码重新取得的系统消息保留
操作系统提供的正文，周围的操作与错误类别仍使用当前 UI locale。

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

只要显式提供任一 owner，本次显式集合就精确替换旧集合：未列出的 Builtin/Rules 本次
不执行且保留既有资产；未列出的 Lua 从新方案移除，并停用 Lua Standard 与 Managed
owner。以下输入承担明确的清除语义：

- Rules 文件的 canonical 内容为 `rule = []` 时，停用 Rules owner，并从以后自动复用的
  Extract 方案中移除 Rules；
- 显式方案未列出 Lua，或零字节 Extract Lua 文件不执行程序，停用 Lua owner、删除该
  owner 的 Standard 与 Managed 快照，并清除 Extract 阶段 Lua 程序；
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
Lua。清除不会猜测或删除 Lua 私有数据库状态。标量和结构化低级准备接口都消费本次解析
出的同一 Profile 与 canonical 资源，不增加 CLI 参数。

Translate 还会读取 `[prompts]`。字段和类型见[配置现行规格](configuration.md#4-prompt语言与-profile)；
资源选择、文件检查、消息装配和模型响应协议见
[Prompt 资源与模型协议现行规格](../rpg-maker/prompts.md)。所选资源或模板无效时，在任何
LLM 请求前失败。

Translate 同时读取 `[rpg_maker].record_translation_tasks`。字段类型和默认值见
[配置现行规格](configuration.md#4-prompt语言与-profile)；记录范围、文件内容、编号以及
记录失败时的处理方式见[翻译任务记录现行规格](../rpg-maker/task-records.md)。

### 3.4 WriteBack

项目从未保存 WriteBack Lua 选择时，省略 `--lua` 使用固定产品行为 Standard-only。
以后省略时复用上次成功选择。非空 Lua 文件替换并启用 WriteBack 阶段程序；零字节文件
清除程序，本轮只执行 Standard，并且不处理 Lua 私有数据库状态。启用 Lua 时，完整
RPG Maker string Value 的受保护写回和低级 `ctx.output` 同时可用；选择接口不增加命令
参数，也不改变本阶段只发布一次的边界。

### 3.5 独立项目 Lua

`lua` 是一次性项目程序入口。`SCRIPT_LUA` 每次必填，ATT 从本次解析后的路径重新读取
完整文件；零字节文件是合法空程序，主 chunk 返回值忽略。脚本、参数和 Profile 选择都
不写入任何阶段运行方案，也不改变 Extract、Translate 或 WriteBack 已保存的 Lua 快照。
`--` 后的参数保持顺序放入
全局 `arg[1..]`，`arg[0]` 是解析后的脚本路径；任一参数不能表示为 UTF-8 时在运行脚本前
明确失败。

独立程序拥有可信 Lua 5.4 标准库（不含本机动态模块装载入口）和公共项目接口。它可打开
Standard 人工候选会话，也可通过 `ctx.translations.edit()` 打开 Managed 人工候选会话；
二者都由相应状态所有者验收和提交，不发送 LLM 请求：

- 显式 `--profile` 在当前配置中精确选择，ID 不存在时失败；
- 未显式选择时，首次打开任一人工会话才读取上次成功 Translate 保存的 Profile；项目没有
  可复用 ID，或保存 ID 已不在当前配置中时，只让该调用失败，不妨碍不使用人工接口的
  普通脚本；
- 显式或复用的 Profile 只服务本次会话，不替换 Translate 保存方案；
- Standard 与 Managed 在同一 VM 中共用一次解析出的 Profile、翻译语义、项目当前
  canonical 术语和 Placeholder，不接受本命令临时覆盖。

一次成功的 Standard 或 Managed accept 已经完成独立短事务提交。脚本之后失败或取消不会
回滚更早已经确认的调用；原游戏目录不被这些接口修改。可信脚本通过标准库或 `ctx.db`
自行产生的其他副作用仍由脚本作者负责。

### 3.6 Lua 快照

Extract、Translate、WriteBack 的非空 Lua 主程序按阶段分别保存在项目数据库中，包括
程序正文 BLOB、SHA-256 和无损 Windows 规范解析路径。自动复用执行保存的正文，不重新
读取原文件；原路径只用于 chunk 名、`require` 搜索目录和诊断。因此主文件移动或被修改
不会改变保存方案。脚本主动加载的模块、文件和进程仍是可信 Lua 的外部动态依赖，不
纳入快照。

独立 `lua` 命令不属于本节快照：它没有复用、清除或保存语义。

### 3.7 成功替换边界

运行方案不是尽力而为的日志，而是后续命令会消费的项目状态。只有业务成功且所有必要
资源（项目日志除外）都完成清理后，ATT 才在最后一个短 SQLite 事务中原子替换本命令的
整套方案：

- 业务失败、取消或必要资源清理失败：不尝试替换；
- 业务已经完整完成后，方案保存等待若被控制信号取消且已确认回滚：旧方案保持，项目
  日志记录本次方案未保存，命令仍完整呈现成功结果并退出 `0`；
- 其他事务确认回滚：旧方案保持不变，退出 `1`；
- 无法确认事务是否提交：退出 `1`，说明业务结果与方案状态不能确认，并建议下次显式传参；
- 业务副作用已经生效但方案保存失败：明确报告“结果已生效、运行方案未保存”，不伪装
  成普通成功。

## 4. 启动与命令构造

一次普通运行按以下顺序执行并结束：

```text
解析 UI locale、进度模式、CLI 意图和必填 --config
  ↓
完整读取一次 TOML，检查 UTF-8、完整语法与未知顶层分区
  ↓
建立已经校验的通用配置
  ↓
建立本命令所需的文件系统、数据库和计算资源
  ↓
取得对应版本的项目租约，读取项目事实和保存方案
  ↓ 工作区合法建立后
建立 RunId 并启动独占项目 JSONL（任一步失败时明确警告并降级）
  ↓
把显式输入、项目状态或产品行为解析为本次完整方案
  ↓
只构造本次命令需要的 Profile、Client、Lua 和其他功能
  ↓
执行命令，清理必要资源；阶段命令原子保存运行方案
  ↓
呈现最终结果；项目日志独立完成剩余写入，不参与业务最终状态判断
```

除项目日志外，本命令所需资源由一个组件统一管理生命周期。Init 按
`FileSystem → SQLite` 建立；Extract、Translate、WriteBack 和独立 `lua` 按
`CPU → FileSystem → SQLite` 建立，并且只在最终选择确实需要 Lua 时继续建立 Lua
Runtime。无论正常完成还是任一步启动失败，已经建立的资源都按
`Lua → SQLite → FileSystem → CPU` 的可用子集逆序关闭；某项资源关闭失败不会阻止其余
资源继续关闭，首要业务或启动失败与全部关闭错误会同时保留。
Extract、Translate 和 WriteBack 显式选择的零字节 Lua 只表达停用或清除，因此不会建立
Lua Runtime；独立 `lua` 的零字节文件则是合法空 chunk，仍建立 Runtime 并执行。

项目租约必须真实取得，不能用普通标记代替。Init、Extract、Translate 和 WriteBack
接收一个生命周期与租约绑定的引用，因此不能在租约释放后继续运行。租约覆盖业务执行、
必要资源关闭和阶段运行方案保存，完成后才释放。Translate 的 LLM 执行器、任务记录专用
FileSystem、保存运行方案时短暂使用的 SQLite，以及项目日志各自管理自己的生命周期，
但不改变上述资源的建立和逆序关闭顺序。

Translate 在选定显式或保存的 Profile 后，精确选择当前配置中的 Profile 及其 Client；
项目开启后取得 metadata 的规范 `LanguagePair`，再按
[Prompt 资源与模型协议现行规格](../rpg-maker/prompts.md)选择和装配本轮 Prompt。只有最终
方案启用某阶段 Lua 时才构造程序固定策略的 Lua Runtime；配置不包含 Lua Runtime 分区。

独立 `lua` 每次从显式路径构造程序。未显式提供 Profile 时，公共 Lua 能力先运行；首次
打开 Standard 或 Managed 人工会话才解析保存的 Translate Profile，并缓存同一份资源与
翻译语义供两个接口复用。该入口不构造 LLM 请求执行器，不建立虚假的 Standard 或 Managed
TaskBlock。

Extract、Translate、WriteBack 与独立 Lua 各自在命令生命周期内构造一个私有 Rayon CPU
池。文档解析、规则扫描、资产编解码、规划准备、人工 Standard 准备与写回计算共享操作
系统可用并行度。资源忙时任务自然等待；等待期间取消的任务不会开始，已经开始的任务
会完成，shutdown 停止接收新任务并等待已经开始的任务结束。

## 5. 进度与输出通道

进度只呈现业务已经确认的绝对事实，不制造全局百分比或 ETA。每个命令拥有自己的
阶段枚举，共享渲染只接收 `Indeterminate` 或 `{completed, total}` 快照：

- Init：检查项目、扫描来源、构建候选、完成数据库更新、发布、保存运行方案使用阶段 spinner；
- Extract：先显示 owner 阶段 `i/N`；文档、Builtin 工作单元和 Rules 规则在真实分母建立
  后显示局部进度，Lua 与 SQLite 提交使用 spinner；
- Translate：规划完成后显示“已确认任务 x/N”，只在该任务所需数据库提交成功后推进；
  Complete、Partial、Unavailable 都计入，零任务提示“无需调用模型”且不显示 `0/0`；
- WriteBack：资产读取、规划和文档改写使用可取得的真实计数；Lua、候选验证和发布使用
  spinner；
- Lua：程序执行和每次 Standard/Managed 人工提交使用 spinner，不制造 LLM 任务进度；
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

业务已经成功时，最终摘要只尝试呈现一次。stdout 写入失败后不重试，也不回滚已经生效的
数据库、候选发布或其他业务副作用；调用方必须假定 stdout 可能已经写出一部分。若此时
还存在非日志 shutdown 失败，stdout 写入错误是 stderr 与项目日志中的 primary，
shutdown 诊断按原顺序作为 related failure 保留，进程退出 `1`。stdout 完整写出而
shutdown 失败时，成功摘要保持有效，随后在 stderr 报告收尾失败并退出 `1`。

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

第一次 Ctrl-C 后停止派生新阶段；SQLite、发布、CPU 和 HTTP 已经开始的工作继续到明确
结果。候选尚未发布时 discard；publish 已开始时等待结果。业务最终取消时不保存运行方案；
Extract、Translate 或 WriteBack 正在执行 Lua 时同时请求该阶段 Lua Runtime 合作取消，Host 先回滚未闭合
的交互事务，WriteBack 再 discard 尚未发布的完整候选。
若信号到达后业务仍自然完整完成，则归入成功路径，必要收尾后照常保存方案并完整呈现
结果。普通项目日志故障不会改变取消与收尾次序。Translate 停止启动新的 Standard/Managed 任务；
每个已发出 `TaskStarted` 的任务仍按自然顺序完成结果处理，并在文件系统 shutdown 前
形成“已提交、未提交或已取消”的明确任务记录。没有启动的任务不生成记录。

## 7. 日志、退出码与安全诊断

项目日志是辅助记录，不参与网络请求、数据库提交、目录发布、恢复、取消或成功退出的
判断。工作区合法建立后，每个 RunId 独占一个 JSONL 文件；日志不共享正在写入的文件，
也不轮转。普通事件写入队列时不等待；队列已满时丢弃本条并计数。失败、性能和运行最终
状态使用单独的可靠写入位置。RunId 建立失败会禁用本次 JSONL 与任务记录。RunId、日志
建立、队列丢弃、写入、flush、sync 或关闭失败时，stderr 在命令结束时最多显示一次项目
日志警告；存在具体原因时继续显示路径、具体操作和清理后的底层原因，但不改变原本的
成功、失败或取消退出码。详细契约见[普通项目日志](project-log.md)。

翻译任务记录是独立于项目 JSONL 的可选文件。哪些任务会被记录、文件写在哪里、保存
哪些内容以及记录失败时如何处理，只由
[翻译任务记录现行规格](../rpg-maker/task-records.md)规定。

项目日志建立后的命令 panic 由命令边界转换成 `internal.operation` 安全诊断：CLI 与
JSONL 都显示实际命令阶段、项目工作区、日志路径和 `outcome_unknown` 影响，绝不显示
panic payload；JSONL 以未知最终状态完成，CLI 退出 `1`。只有日志建立前或日志无法建立时的
panic 才由进程启动边界直接写 stderr。完整 Clap 解析已经确认 UI locale 时，启动边界
使用该 locale；解析完成前固定使用英语，绝不从尚未通过参数 schema 的预扫描值选择语言。

| 退出码 | 含义 |
|---|---|
| `0` | Help、Version 或命令成功，包括正常 Partial/Unavailable |
| `2` | CLI 解析错误 |
| `1` | 配置、输入、业务或技术错误；必要资源 shutdown 失败；运行方案保存失败或结果未知 |
| `130` | Ctrl-C 后完成受控收尾；日志故障仍保持 `130` |

错误统一说明错误码、阶段、对象或路径、具体原因、稳定底层代码、状态影响、处理办法和
恢复位置。OS 系统消息、SQLite primary/extended code、HTTP 状态与允许公开的供应商
code/type 在清理控制字符后按实际存在的字段明示；缺失字段不打印占位文字。不得用笼统
类别替代具体原因。输出不读取任意 `Debug`
或内部来源链，并采用
[Chat Completions 规格规定的敏感信息边界](chat-completions.md#6-敏感信息闭集唯一权威)。
CLI 与普通 JSONL 不复制配置原文、Header、完整 Client parameters 或完整模型任务正文，
是为了维持职责、稳定 schema、可读体积和控制字符边界，不构成敏感性分类。任务记录的
可读正文和精确替换同样以该权威契约为准。
