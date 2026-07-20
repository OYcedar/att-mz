# ATT 配置编写与运行能力导航

本文帮助使用者根据当前命令和运行环境编写 ATT 配置。它不是另一份字段规格，也不提供
一份适合所有机器、游戏和模型服务的万能配置。字段形状以仓库根目录的
[`config.example.toml`](../../config.example.toml) 为参考，精确约束以
[生产配置现行规格](configuration.md)为准。

配置的作用是把路径、资源预算和外部服务选择显式交给真实消费者。Init 所需的游戏根、
语言对和三类布局宽度仍来自命令输入或已有项目状态；提取规则、术语、占位符规则与 Lua
文件也是命令输入，不应搬进生产配置。

## 从当前命令反推配置范围

除 Help 和 Version 外，每次运行都必须显式传入 `--config FILE`，不存在默认配置路径。
最小配置不是固定文件，而是当前命令实际选择的分区中，每个必填字段都完整存在。

所有 Init、Extract、Translate 和 WriteBack 都会选择以下基础分区：

- `projects.root`；
- `runtime.async`；
- `runtime.filesystem` 中的文件读取、目录枚举、目录树和项目锁配置；
- `runtime.sqlite`；
- `observability.root` 与 `observability.audit`。

再根据命令与显式选项补充：

| 当前命令 | 额外选择的配置 |
|---|---|
| Init | `runtime.filesystem.publisher` |
| Extract | `runtime.cpu`、`rpg_maker.document`、`rpg_maker.extract.store` |
| Translate | `prompts.root`、全部 `languages`、`runtime.cpu`、`runtime.llm`、所选 RPG Maker Profile、该 Profile 引用的 LLM Client、`rpg_maker.standard_asset`、`rpg_maker.translate.store` |
| WriteBack | `runtime.cpu`、`runtime.filesystem.publisher`、`rpg_maker.document`、`rpg_maker.standard_asset` |
| 任一显式带 `--lua` 的阶段 | 另选 `runtime.lua`；配置中存在该分区不会自行启用 Lua |

`config.example.toml` 展示的是这些能力的并集。可以保留一份完整配置，也可以按部署目的
维护较小的配置；两种方式都应从将要执行的命令反推必填内容，而不是靠删字段试出最小
文件。完整 TOML 始终要通过语法、重复 key 和已知顶层分区检查；已知但未被当前命令选择
的分区内部不会被解析或校验。

命令和选项的现行形状见[运行时与 CLI](cli.md)。面对未知游戏时如何判断应选择 Builtin、
Rules 或 Lua，见 [RPG Maker 调查与决策指南](../rpg-maker/README.md)。

## 分清三种路径基准

路径问题常常不是字段写错，而是相对路径参照物不同：

| 路径来源 | 相对路径基准 | 例子 |
|---|---|---|
| `--config FILE` | 进程当前工作目录 | `--config settings/att.toml` |
| 配置文件内部的路径值 | 配置文件所在目录 | `projects.root`、`prompts.root`、`observability.root`、额外 PEM 文件 |
| 其他 CLI 文件或目录参数 | 进程当前工作目录 | Init 的游戏根，以及 Rules、MV 对话规则、术语、占位符规则和 Lua 文件 |

因此，把配置移到另一个目录会改变配置内相对路径的含义；从另一个目录启动 ATT 会改变
相对 CLI 路径的含义。需要跨目录复用命令时，使用明确的绝对路径通常更容易验证。

项目工作区由 ATT 从配置与命令共同派生：

```text
<projects.root>/<engine>/<project-name>
```

这里外部字段仍名为 `engine`，值只能是 `mv | mz`；MV 和 MZ 都属于当前 RPG Maker
领域。不要在配置中另写项目工作区、锁目录或 `write_back` 目录。

## 配置 Translate 的引用链

Translate 的选择不是“找到一组看起来相关的字段”，而是一条可逐段核对的引用链：

```text
项目 metadata 中的规范 LanguagePair
  ├─ source ──> [[languages]] 中同 ID 的源语言模块
  └─ source + target ──> <prompts.root>/rpg_maker/<source>--<target>.md

命令行 PROFILE_ID
  └─> [[rpg_maker.translation_profiles]] 的精确 id
        └─ llm_client ─> [llm.clients.<id>]
```

### 语言模块与 Prompt

当前实现提供 `japanese` 与 `english` 两类源语言模块。`[[languages]]` 中的每个条目都会在
Translate 启动时解析和校验；ID 先按当前语言标签契约规范化，再进行精确匹配，不做父语言、
别名或目录首项回退。项目目标语言不要求存在同类型的源语言模块，但项目的源语言必须能
精确选中一个模块。

项目语言对来自 Init 已写入的 metadata，不由 Profile 或配置文件重新指定。Prompt 也不在
Profile 中建立映射，而是按该语言对读取唯一文件：

```text
<prompts.root>/rpg_maker/<source>--<target>.md
```

例如规范语言对为 `ja` 到 `zh-Hans` 时，读取 `ja--zh-Hans.md`。ATT 不提供默认 Prompt
正文，也不尝试大小写变体、父语言或备用文件。Prompt 的协议要求见
[翻译现行规格](../rpg-maker/translation.md)。

### Profile 与 Client

命令行位置参数 `PROFILE_ID` 精确选择一个 RPG Maker Profile，不 trim、不折叠大小写，
也没有默认 Profile。Profile 保存翻译规划、任务并发和重试策略，并通过 `llm_client` 精确
引用一个公共 Client；网络地址、模型与凭据只由 Client 拥有。

当前 Client 执行非流式、OpenAI-compatible Chat Completions。`parameters` 是 TOML 字符串
中承载的严格 JSON 对象，不是 TOML 内联表；它不能再提供 `model`、`messages` 或
`stream`。服务端专有参数可以放入该对象，但 ATT 不会解释或修正它们。不要把 Responses
API、流式协议或供应商自动探测当作现有能力。请求与响应边界见
[Chat Completions 运行根](chat-completions.md)。

## 根据环境确定资源值

示例中的数值只展示合法字段形状，不是默认值或性能建议。资源值应能解释当前机器、
存储、代表性游戏与模型服务的现实限制，并通过运行结果再校准。

| 配置范围 | 先观察什么 | 调整后验证什么 |
|---|---|---|
| `runtime.async` | 进程级异步调度需要的线程与阻塞任务上限 | 文件、审计和网络任务在负载下仍能及时推进 |
| `runtime.cpu` | 可用 CPU、解析/扫描/编解码负载、可接受的等待量 | 吞吐提升是否真实，等待队列与内存是否仍有界；`worker_threads` 只用 `"auto"` 或正整数 |
| `runtime.filesystem` 与 `.tree` | 最大普通文件、单目录条目数、来源树深度/总量 | 合法游戏有余量，同时错误游戏根或异常输入仍能被预算阻止 |
| `runtime.sqlite` | 数据库规模、查询结果、繁忙等待与持久性要求 | 连接、查询和落盘在真实项目上成立；Init 的快照以及任一显式带 `--lua` 的 Extract、Translate 或 WriteBack 至少需要两个开放连接 |
| `runtime.llm` | 服务并发、RPM、突发额度、连接和响应延迟 | 不靠无限排队掩盖限流；429、超时、吞吐和取消行为符合服务现实 |
| RPG Maker Profile | 单任务消息上限、期望并发、供应商重试建议 | `max_in_flight_tasks` 不超过 LLM 活动与排队总容量，重试不会放大拥塞 |
| 文档与 Store 批量粒度 | 单份游戏材料的大小分布、CPU 固定开销与峰值内存 | 较大批次确实减少开销，且不破坏内存边界、取消响应和结果确定性 |
| 审计与发布 | 审计记录速率、磁盘容量、锁等待、恢复产物需求 | 强审计能持久化，Init/WriteBack 发布失败时仍可明确恢复 |

同时把互相约束的值一起看待。提高 Profile 并发而不检查 LLM 总容量和供应商限额不会增加
有效吞吐；扩大队列只会增加等待与内存；把所有线程数同时调高也可能让 CPU、磁盘和
SQLite 彼此争用。审计记录、SQLite 行为和命令终态比单次耗时更能说明调整是否成立。

SQLite 持久策略、查询预算与并发语义见 [SQLite 运行时](sqlite.md)，强审计语义见
[强审计账本](audit-log.md)，目录发布和恢复边界见
[Windows 文件能力与可恢复目录发布](directory-publishing.md)。

## 密钥、网络代理与证书

`llm.clients.<id>.api_key` 是配置中的实际字符串，当前不会展开环境变量，也没有外部密钥
提供器。把 `api_key = "$NAME"` 写进 TOML 只会把字面值 `$NAME` 当作密钥发送。需要真实
凭据时，应使用不纳入版本控制的本地配置并限制文件访问；提交的示例只能保留占位值。

ATT 会避免把 API key、完整 Client parameters 和 Prompt 内容写入错误、审计或调试输出，
但这不改变配置文件本身含有秘密的事实。`runtime.llm.proxy` 可设为 `false` 或合法的网络
代理 URL，URL 不得内嵌凭据。`runtime.llm.tls.additional_pem_files` 中的相对路径以配置
目录为基准，应指向部署环境实际信任的 PEM 文件。

## 编辑后应能回答的问题

配置是否合适，不由“能解析 TOML”单独证明。根据当前操作，可以选择足以回答这些问题
的验证：

- 当前命令为何需要这些分区，是否误把未使用能力当成必需配置；
- 每个路径最终解析到了哪个绝对位置；
- Translate 的源语言模块、Prompt、Profile 与 Client 是否分别精确命中；
- 资源上限是在保护真实负载，还是因复制示例值意外拒绝或放任输入；
- 模型服务的并发、限流、超时、重试和 `parameters` 是否与其实际契约一致；
- 命令成功、正常的 Partial/Unavailable、配置失败和技术失败是否被正确区分；
- 审计、项目数据库与候选发布是否提供了与本次副作用相称的证据。

初始化事实的取得方式见 [RPG Maker 初始化现行规格](../rpg-maker/init.md)，Builtin、Rules
与 Lua 的选择见[文本提取现行规格](../rpg-maker/extraction.md)，写回候选和发布语义见
[写回现行规格](../rpg-maker/write-back.md)。

## 常见误区

- 把 `config.example.toml` 的值当成程序默认值，或认为省略字段后会自动推断；
- 忘记 `--config` 相对当前工作目录，而配置内路径相对配置文件目录；
- 认为完整 TOML 语法通过，就表示每个未选择分区也已完成语义校验；
- 试图在配置中填写 Init 的源语言、目标语言或三类宽度；
- 把 Rules、术语或占位符文件放进配置分区，而不是通过对应 CLI 选项选择；
- 假设 Profile 会按语言自动选择，或 Prompt、语言模块存在默认/父语言回退；
- 在 Client `parameters` 中重复 `model`、`messages`、`stream`，或使用不是完整 JSON 对象的内容；
- 期待 API key 环境变量插值，或把含真实密钥的完整配置提交到仓库；
- 独立放大线程、队列和并发值，却不核对总容量、供应商限流、内存和磁盘证据；
- 仅因配置中存在 `runtime.lua` 就认为 Lua 会运行；Lua 只由相应命令的 `--lua` 显式选择。
