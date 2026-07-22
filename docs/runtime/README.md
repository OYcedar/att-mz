# ATT 配置编写与运行能力导航

本文帮助使用者根据当前命令和运行环境编写 ATT 配置。字段形状以仓库根目录的
[`config.example.toml`](../../config.example.toml) 为参考，精确约束以
[生产配置现行规格](configuration.md)为准，命令省略语义以
[运行时与 CLI](cli.md)为准。

配置负责建立路径、资源预算和外部服务选择。项目来源、语言对、布局宽度、提取 owner、
Profile、术语、Placeholder、MV 对话定义和 Lua 程序属于 CLI 意图或项目保存状态，不能
为了“省参数”搬进生产配置。

## 从当前命令反推配置范围

除 Help 和 Version 外，每次运行都必须显式传入 `--config FILE`；`mv | mz` 和 `--name`
同样必填。`--ui-language`、`ATT_UI_LANGUAGE` 与 `--progress auto|plain|off` 不属于
配置文件；前两者决定本进程有效 UI locale，并在 `prompts.locale = "auto"` 时同时选择
同 locale 的外置 Prompt，`--progress` 只控制终端进度呈现。

所有 Init、Extract、Translate 和 WriteBack 都会选择：

- `projects.root`；
- `runtime.async`；
- `runtime.filesystem` 中的文件读取、目录枚举、目录树和项目锁配置；
- `runtime.sqlite`；
- `observability.root` 与 `observability.log`。

再根据解析后的完整运行方案补充：

| 当前命令 | 额外选择的配置 |
|---|---|
| Init | `runtime.filesystem.publisher` |
| Extract | `runtime.cpu`、`rpg_maker.document`、`rpg_maker.extract.store` 和本轮可执行 owner 所需能力 |
| Translate | `[prompts]` 的 `root`、`locale`、`thinking_output`，全部 `languages`、`runtime.cpu`、`runtime.llm`、显式或保存的 Profile、该 Profile 引用的 Client、`rpg_maker.standard_asset`、`rpg_maker.translate.store` |
| WriteBack | `runtime.cpu`、`runtime.filesystem.publisher`、`rpg_maker.document`、`rpg_maker.standard_asset` |
| 解析后启用任一阶段 Lua | `runtime.lua`；配置中存在该分区不会自行启用 Lua |

`config.example.toml` 展示能力并集。可以维护一份完整配置，也可以按部署目的维护较小配置；
两种方式都应从将执行的方案反推必填内容。完整 TOML 始终接受语法、重复 key 和已知顶层
分区检查；已知但未被当前方案选择的分区内部不会被解析或校验。

面对未知游戏时如何选择 Builtin、Rules 或 Lua，见
[RPG Maker 调查与决策指南](../rpg-maker/README.md)。

## 正确理解“省略参数”

省略不表示 ATT 猜一个默认值。它表示从项目中读取已经成功建立的事实，或使用文档明确
规定的固定产品行为。终端提示和最终摘要会说明来源。

| 命令 | 省略时发生什么 |
|---|---|
| Init | 已有项目省略 `--path` 复用上次成功来源；语言和宽度逐项复用 metadata |
| Extract | 三个 owner 选项全部省略时复用上次成功完整 owner 方案；显式给出任一 owner 时精确替换整个集合 |
| Translate | 省略 `PROFILE_ID` 复用上次成功 Profile；术语和 Placeholder 复用 canonical 项目资源；Lua 复用阶段快照 |
| WriteBack | 从未保存 Lua 选择时省略表示 Standard-only；以后复用上次成功选择 |

清除也必须是结构化意图：Rules 的 `rule = []`、术语的 `term = []`、Placeholder 和 MV
对话定义的 `rule = []` 分别清空对应语义；零字节 Lua 文件清除相应阶段程序。Extract 的
零字节 Lua 还停用 Lua owner 并删除其标准资产，Translate/WriteBack 则不处理 Lua 私有
数据库状态。

非空 Lua 主程序会按 Extract、Translate、WriteBack 阶段分别保存正文快照。以后复用不
重新读取主文件，因此移动或修改原文件不会改变已保存程序；原解析路径仍决定 chunk 名、
`require` 搜索目录和诊断。脚本主动加载的外部依赖仍是动态的。

运行方案只在业务与必要非日志收尾成功后原子替换。方案保存失败会使命令失败并准确说明
业务结果是否已经生效；普通项目日志失败不会影响这项事务或退出码。

## 分清三种路径基准

| 路径来源 | 相对路径基准 | 例子 |
|---|---|---|
| `--config FILE` | 进程当前工作目录 | `--config settings/att.toml` |
| 配置文件内部的路径值 | 配置文件所在目录 | `projects.root`、`prompts.root`、`observability.root`、额外 PEM 文件 |
| 其他 CLI 文件或目录参数 | 进程当前工作目录 | Init 游戏根，以及 Rules、MV 对话规则、术语、Placeholder 和 Lua 文件 |

项目工作区由 ATT 派生：

```text
<projects.root>/<engine>/<project-name>
```

`engine` 只能是 `mv | mz`。不要在配置中另写项目工作区、锁目录或 `write_back` 目录。

显式 Lua 路径解析后以无损 Windows 表达写入数据库；主程序正文是复用时的权威内容，保存
路径只承担 Lua 加载语境。Rules、术语和 Placeholder 保存 canonical 语义，不复用原输入
路径。

## 配置 Translate 的引用链

Translate 的 Profile ID 可以来自 CLI，也可以来自上次成功项目方案，但最终都必须精确
命中当前配置：

```text
显式或项目保存的 PROFILE_ID
  └─> [[rpg_maker.translation_profiles]] 中精确 id
        └─ llm_client ─> [llm.clients.<id>]

项目 metadata 中的规范 LanguagePair
  ├─ source ─> [[languages]] 中精确 ID 的源语言模块
  └─ source/target ─> system.md 的两个 LanguageId 模板变量

[prompts].locale
  ├─ auto ─> 本进程已经解析的有效 UI locale
  └─ 显式值 ─> 覆盖 UI locale
        └─> <prompts.root>/rpg_maker/<locale>/system.md
              └─ thinking_output = true 时追加同目录 thinking.md
```

保存的 Profile 不存在时 ATT 明确失败，不选择目录第一项、不按语言猜测，也不回退其他
Profile。Prompt locale 规范化后也不尝试父语言、中文、英文、目录首项、旧语言对文件或
其他备用资源；关闭思考输出时不读取 `thinking.md`。当前 Client 执行非流式、
OpenAI-compatible Chat Completions；请求与响应边界见
[Chat Completions 运行根](chat-completions.md)。

游戏翻译使用的开放 `LanguageId` 与终端使用的闭集 `UiLocale` 不是同一种配置。终端支持
`ar / zh-Hans / zh-Hant / en / fr / ru / es / ja / ko / vi`。UI locale 自动检测不会改变
项目 LanguagePair；只有配置明确选择 `prompts.locale = "auto"` 时，它才成为本轮 Prompt
locale。提示词模板仍使用项目规范 LanguageId 建立实际翻译方向。

## 根据环境确定资源值

示例数值只展示合法字段形状，不是默认值或性能建议。资源值应能解释当前机器、存储、
代表性游戏与模型服务的现实限制，并通过运行结果再校准。

| 配置范围 | 先观察什么 | 调整后验证什么 |
|---|---|---|
| `runtime.async` | 进程级异步调度需要的线程与阻塞任务上限 | 文件与网络任务在负载下仍能及时推进 |
| `runtime.cpu` | 可用 CPU、解析/扫描/编解码负载、可接受等待量 | 吞吐收益真实且等待队列、内存仍有界 |
| `runtime.filesystem` 与 `.tree` | 最大普通文件、单目录条目、来源树深度与总量 | 合法游戏有余量，错误根或异常输入仍被预算阻止 |
| `runtime.sqlite` | 数据库规模、查询结果、繁忙等待与持久性要求 | 连接、查询、Lua 会话和运行方案事务在真实项目成立 |
| `runtime.llm` | 服务并发、RPM、突发额度、连接和响应延迟 | 不靠无限排队掩盖限流，超时、吞吐和取消符合服务现实 |
| RPG Maker Profile | 消息上限、期望并发、供应商重试建议 | Profile 并发不超过 LLM 活动与排队总容量 |
| 文档与 Store 批量粒度 | 材料大小分布、CPU 固定开销与峰值内存 | 批量确实减少开销且不破坏取消和确定性 |
| `observability.log` | 可接受的日志密度、磁盘预算和跨进程锁竞争 | 日志有界轮转；故障注入时业务结果和退出码保持不变 |

进度条只报告真实建立的分母和已提交计数，不能用显示刷新速度判断模型吞吐。Translate
达到 `N/N` 后仍会进入收尾与保存方案阶段；`auto` 在非 TTY 中完全静默，自动化环境若
需要稀疏进度行应显式使用 `--progress plain`。

SQLite 持久策略、查询预算与并发语义见 [SQLite 运行时](sqlite.md)，日志语义见
[普通项目日志](project-log.md)，目录发布和恢复边界见
[Windows 文件能力与可恢复目录发布](directory-publishing.md)。

## 密钥、网络代理与证书

`llm.clients.<id>.api_key` 是配置中的实际字符串，当前不会展开环境变量。需要真实凭据时，
使用不纳入版本控制的本地配置并限制文件访问；提交示例只能保留占位值。

ATT 不把 API key、Header、完整 Client parameters、Prompt、messages、模型正文、原文或
译文写入错误、终端或项目日志，但这不改变配置文件本身含有秘密的事实。
`runtime.llm.proxy` 可设为 `false` 或合法代理 URL，URL 不得内嵌凭据。额外 PEM 文件的
相对路径以配置目录为基准。

## 编辑后应能回答的问题

- 当前命令为何需要这些分区，是否误把未使用能力当成必需配置；
- 每个路径最终解析到了哪个绝对位置；
- 本次每项运行选择来自显式输入、项目状态还是固定产品行为；
- Translate 的源语言模块、规范 Prompt locale、实际资源、响应信封、Profile 与 Client
  是否分别精确命中；
- 资源上限是在保护真实负载，还是因复制示例值意外拒绝或放任输入；
- Complete、Partial、Unavailable、配置失败和技术失败是否被正确区分；
- 项目数据库与候选发布是否提供了与副作用相称的权威状态；
- 日志不可用时，业务结果、运行方案和退出码是否完全不变。

初始化、提取、翻译和写回的领域契约分别见 `docs/rpg-maker` 下的现行规格。

## 常见误区

- 把 `config.example.toml` 的值当成程序默认值；
- 把“复用上次成功方案”理解为再次读取旧 Rules、术语或 Lua 主文件；
- 显式提供一个 Extract owner，却期待未列出的旧 owner 仍自动执行；
- 认为保存的 Profile 不存在时会自动选用其他 Profile；
- 仅因配置中存在 `runtime.lua` 就认为 Lua 会运行；
- 把 Prompt locale 或 UI locale 与项目翻译语言混为一谈；
- 认为关闭 `thinking_output` 时 ATT 仍会读取或校验 `thinking.md`；
- 认为 `--progress off` 会关闭最终结果、错误或项目日志；
- 期待 API key 环境变量插值，或把真实密钥提交到仓库；
- 独立放大线程、队列和并发值，却不核对供应商限流、内存和磁盘证据；
- 把普通项目日志当作数据库或目录发布的恢复依据。
