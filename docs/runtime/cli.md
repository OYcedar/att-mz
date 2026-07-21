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

用户控制的路径、ID 和其他动态文本在进入终端或日志前会移除 ESC、换行伪装及既有
双向文本控制字符。阿拉伯语消息保持逻辑顺序；命令、路径、ID、数字和进度条作为独立
方向片段渲染，不反转原字符串。

## 3. 运行方案解析与持久化

项目数据库为四个命令分别保存上次成功运行方案。项目租约覆盖方案读取、业务执行、
必要收尾和最终方案替换，防止两个并发命令把不同选择拼成同一方案。

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

### 3.4 WriteBack

项目从未保存 WriteBack Lua 选择时，省略 `--lua` 使用固定产品行为 Standard-only。
以后省略时复用上次成功选择。非空 Lua 文件替换并启用 WriteBack 阶段程序；零字节文件
清除程序，本轮只执行 Standard，并且不处理 Lua 私有数据库状态。

### 3.5 Lua 快照

Extract、Translate、WriteBack 的非空 Lua 主程序按阶段分别保存在项目数据库中，包括
程序正文 BLOB、SHA-256 和无损 Windows 规范解析路径。自动复用执行保存的正文，不重新
读取原文件；原路径只用于 chunk 名、`require` 搜索目录和诊断。因此主文件移动或被修改
不会改变保存方案。脚本主动加载的模块、文件和进程仍是可信 Lua 的外部动态依赖，不
纳入快照。

### 3.6 成功替换边界

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
受限读取一次 TOML，检查完整语法与未知顶层分区
  ↓
建立通用受信配置并尝试启动普通项目日志（失败时使用 no-op）
  ↓
取得对应版本的项目租约，读取项目事实和保存方案
  ↓
把显式输入、项目状态或产品行为解析为本次完整方案
  ↓
只构造本方案消费的 Profile、Client、Lua 和纵向根能力
  ↓
执行命令，完成必要非日志收尾，原子保存运行方案
  ↓
呈现最终结果；项目日志独立尝试排空，不参与业务终态
```

Translate 在选定显式或保存的 Profile 后，精确选择当前配置中的 Profile 及其 Client；
项目开启后按 metadata 的规范 `LanguagePair` 读取
`<prompts.root>/rpg_maker/<source>--<target>.md`。只有最终方案启用某阶段 Lua 时才构造
Lua Runtime；配置中存在 `runtime.lua` 本身不会启用程序。

Extract、Translate 与 WriteBack 各自在命令生命周期内构造一个私有 Rayon CPU 池。
文档解析、规则扫描、资产编解码、规划准备与写回计算共享同一线程和准入预算。等待 CPU
准入时取消则任务不执行；已经准入的任务会完成，shutdown 停止新准入并排空已接管作业。

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

同一版本同一项目的四个命令互斥；不同版本的同名项目独立。锁顺序固定为“项目租约 →
目录发布锁 → SQLite/session”。超时返回稳定的“项目不存在或正忙”结果，不继续副作用。

第一次 Ctrl-C 后停止派生新阶段；SQLite、发布、CPU 和 HTTP 已接管的工作继续到明确终态。
候选尚未发布时 discard；publish 已开始时等待终态。运行方案不在取消路径保存。普通项目
日志故障不会改变取消与收尾次序。

## 7. 日志、退出码与安全诊断

项目日志是不可失败的可观测性旁路，不是网络请求、数据库提交、目录发布、恢复、取消或
成功退出的门禁。启动、排队、写入、轮转、保留和关闭故障最多产生一次本地化警告，不
改变原本的成功、失败或取消退出码。详细契约见[普通项目日志](project-log.md)。

| 退出码 | 含义 |
|---|---|
| `0` | Help、Version 或命令成功，包括正常 Partial/Unavailable |
| `2` | CLI 解析错误 |
| `1` | 配置、输入、业务或技术错误；非日志 shutdown 失败；运行方案保存失败或终态未知 |
| `130` | Ctrl-C 后完成受控收尾；日志故障仍保持 `130` |

错误统一说明“发生了什么、影响是什么、如何处理”。用户可修复错误只呈现清理后的路径、
行列、字段、稳定原因和建议，不输出任意 `Debug`、内部来源链、配置原文、API key、Header、
Client parameters、Prompt、完整模型消息、模型正文、原文或译文。
