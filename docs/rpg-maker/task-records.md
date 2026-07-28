# RPG Maker 翻译任务记录现行规格

## 1. 目的、范围与开关

任务记录是供人工和 Agent 排查问题的 Markdown 文件。一个文件包含一个 TaskBlock 的最终
输入、全部请求尝试、模型输出、每个 ID 的译文检查结果和数据库写入结果。只有模型协议和
数据库写入都由 ATT 完整管理时，ATT 才能生成这样的记录。它覆盖 RPG Maker Standard 与
Lua Managed，不是逐次 HTTP 抓包，也不用于恢复、重放或判断项目数据库状态。

该能力默认关闭，只由 Translate 消费：

```toml
[rpg_maker]
record_translation_tasks = false
```

`record_translation_tasks` 可省略，默认 `false`。它不属于某个 Client，不进入翻译
state、保存的运行方案或项目数据库。开启后记录本轮 Standard TaskBlock，以及 Translate
Lua 通过 `ctx.translations.translate()` 建立的 Managed TaskBlock。低级 `ctx.llm` 不生成
记录，因为其响应格式、逐 ID 检查和数据库写入由 Lua 脚本负责。

## 2. 路径、身份与生命周期

文件固定写入：

```text
<project-workspace>/task-records/<run-id>/task-000001.md
```

文件序号来自本轮 Translate 中统一递增的任务序号，不来自 HTTP 发送或响应完成顺序。
Standard 任务使用前面的序号，Managed 任务接着编号；两边各自的计划顺序保持不变。
同一个 TaskBlock 的全部重试归入同一个文件。关闭记录，或者 Standard 与 Managed 都没有
启动任何任务时，不建立空的 `task-records/<run-id>` 目录。

任务记录身份依赖本次运行已经成功建立的 RunId。RunId 建立失败时，即使配置已开启记录，
本次任务记录也整体禁用，不创建目录或文件；stderr 只报告一次导致 RunId 无法建立的项目
日志问题，不再把同一个问题重复计为任务记录故障。

每个已经发出 `TaskStarted` 的任务最终只有一份结果记录。合作取消后停止启动新任务；
已经启动的任务仍会完成结果处理并生成记录，尚未启动的任务不生成文件。全部任务记录
写入结束后，ATT 才能关闭本轮使用的文件系统资源。

## 3. Markdown 信息结构

参考中文呈现固定为：

~~~markdown
# 翻译任务 000030 · 部分完成

`任务 30/128` · `尝试 2 次` · `验收 14/17`

- Run ID：`...`
- 开始时间：`...`
- 总耗时：`...`
- Endpoint：`...`
- Model：`...`

## Managed

- Collection: `quest_titles`
- ID → collection/key:
  - `1` → `quest_titles`/`quest:arrival`
  - `1` → `shared_quest_titles`/`quest:arrival-copy`
  - `2` → `quest_titles`/`quest:return`

## 自定义参数

```json
{
  "temperature": 0.2
}
```

## System

<原生 Markdown>

## User

<原生 Markdown>

## 请求过程

- 尝试 1：HTTP 429；等待 1.5 秒后重试
- 尝试 2：成功；finish reason `stop`；token `1200 / 320 / 1520`

## Thinking

<why> 内部正文按原生 Markdown 渲染，标题标签本身不显示。

## Assistant

### ID 1

第一项译文

### ID 2

第二项译文

## 最终结果

- 状态：部分完成，已确认提交
- Managed checkpoint: `partial`
- IDs: accepted `14/17`; confirmed committed unit targets `15`
- Unit acceptance:
  - ID `1` → collection `quest_titles`, key `quest:arrival`: `accepted`
  - ID `1` → collection `shared_quest_titles`, key `quest:arrival-copy`: `accepted`
  - ID `15` → collection `quest_titles`, key `quest:other`: `rejected`; reason `placeholder_mismatch`
- 任务诊断 `unknown_id`：模型额外返回了未知 ID `99`
~~~

`## Managed` 及其 Collection、ID→collection/key 映射只在 Managed 记录中存在；
Standard 继续显示其标准逻辑身份与传播位置。Managed 的 collection name、unit key、
metadata、来源路径、state 与去重信息都不进入 User；这些身份只保存在 ATT 已经校验的
任务计划中，并写入记录的元数据和最终结果。最终结果逐 ID 显示译文检查结果，并把
Applied、NotApplied、OutcomeUnknown、取消或前序失败等 checkpoint 结果分开显示。

Managed 的验收分母和 accepted 数量按模型临时 ID 计数；`confirmed committed unit
targets` 按数据库中已经确认提交的真实 unit 目标计数。跨 collection 全局去重后，一个
模型 ID 可以扇出到多个 collection/key，因此已提交 target 数可以大于 accepted ID 数。
身份映射和最终检查会逐个展开这些目标，不把多个目标隐藏成一个代表 unit。

`System`、`User` 与输入历史中的 `Assistant` 按原始消息顺序直接作为 Markdown 渲染；
不得把 messages 再包装成 JSON。固定角色标题使用 `System`、`User`、`Thinking`、
`Assistant`，其余标题、状态和诊断使用本次 UI locale。原始 Markdown 中的标题、列表、
引用、表格和代码块保持自身语义。

自定义参数是所选 Client 的实际 JSON 数据，只在本节使用 JSON 围栏。Endpoint、Model
和 parameters 直接来自本次实际选中的 Client，不从请求 JSON 反向解析。任务记录不采集
HTTP Header、完整 Provider 外层 JSON 或非 200 原始 response body。

## 4. Thinking 与 Assistant

只有当前模型协议接受了恰好一个合法、非空、非嵌套的 `<why>...</why>` 信封时，任务记录
才会显示 Thinking。
合法时只渲染标签内部正文，信封标签本身不显示；没有 Thinking 时省略整节。空白、嵌套、
重复、顺序非法或其他无效信封不猜测拆分。

合法 Assistant JSON 不展示 JSON 外层对象，只按模型原始条目顺序，以 `### ID <id>` 展开
译文内容；字符串数组的成员按原顺序各占一段。重复、未知或其他非法 ID 仍完整呈现，并在
“最终结果”中给出协议诊断；缺失 ID 也在最终结果中明确列出。业务验收与任务记录共享
响应解析器返回的同一份结构化结果；记录生成器不重新猜测 `<why>`，也不重新实现 ID、
Placeholder 或语言检查规则。

无效 JSON 或无效 Thinking 信封时，`Assistant` 节使用足以包住正文的动态 Markdown
围栏保留完整原始内容，不因正文自身含反引号而截断；同时只显示响应解析器产生的解析
错误，以及从 1 开始计算的准确行列。Thinking 和原始 Assistant 只写入任务记录，不进入
`TranslationTaskOutcome`、译文数据库或状态指纹。

## 5. 尝试与最终状态

“请求过程”记录每次请求尝试的开始时间、耗时及结构化结果。成功项可以包含 finish reason、
token、Provider request/response ID；失败项可以包含 HTTP 状态、Provider code/type、
`Retry-After`、等待事实和本地化的类型化原因，不显示诊断 JSON 外壳。只有下一次模型
请求确实开始后，上一项才写
“等待后重试”；等待期间取消时写计划等待及取消，等待已经完成但下一次请求未开始时只写
等待完成，不得声称发生了重试。不得保存原始 Header、任意错误 response body，也不得靠解析
错误显示文本补猜这些事实。

“总耗时”从该任务发出 `TaskStarted` 开始，到按自然顺序完成译文检查、确认数据库写入
结果并确定任务状态为止。它不在 Executor 返回时提前停止，也不包含 Markdown 记录本身的
写入时间。

每个已启动任务最终只能是下列状态之一：

- 完成并确认提交；
- 部分完成并确认提交；
- 不可用且项目未改变；
- 执行失败且未提交；
- 提交准备失败或事务确定未应用；
- 提交结果未知；
- 因前序任务失败而未提交；
- Executor 结果序列无效；
- 已取消且未提交。

Complete 或 Partial 只有在对应事务确认成功后才能成立。提交结果未知时不得显示“写入
0 处”或暗示项目未改变。一个任务已经确认提交后，不会因后续 Result Store 会话关闭、
Translate Lua、运行方案保存或本轮其他清理工作失败而改变该任务的状态；这些问题会单独
作为本轮运行问题报告。

## 6. 敏感信息与精确替换

任务记录严格采用
[Chat Completions 规格中的敏感信息清单和替换规则](../runtime/chat-completions.md#6-敏感信息闭集唯一权威)，
不得自行增加需要隐藏的内容。所有可读字段都在生成 Markdown 前应用该规则；任务记录不
采集 HTTP Header，替换时也不能删除命中值以外的字段、段落或相邻正文。

普通 CLI、JSONL 与 Debug 继续使用各模块返回的结构化错误信息，不遍历任意错误链、复制
大段正文或任意控制字符。这些限制用于保持 schema 稳定、输出清楚且大小可控，不会增加
新的敏感信息类别。

## 7. 记录写入失败时的处理

ATT 在译文检查和数据库写入结果都已确认后，为每个已启动任务建立一份内容固定的文档。
Standard、Translate Lua、必要的资源清理和运行方案保存都确定结果后，ATT 再等待任务记录
文件并发写入完毕。记录写入缓慢或失败不能阻塞数据库提交、Translate Lua 或运行方案，
也不能改变何时响应取消。建立内容、生成 Markdown、写文件、清理临时文件或关闭记录专用
文件系统资源失败时，stderr 必须显示最终路径、失败操作、主要错误，以及存在时的清理
错误；这些失败不得改变翻译结果、数据库、退出码、重试、后续任务或运行方案。

任务记录建立、生成、写入、清理或关闭失败时，按“失败的记录操作”计数；同一次操作的
主要错误和清理错误都要保留。任务记录与项目 JSONL 分别统计失败，并分别在本次命令结束
时最多显示一次提示；其中一类较早发生的失败不能覆盖另一类。

文件先在目标目录写入同目录临时文件，完成 `write_all + flush + close` 后，再以不覆盖
方式执行原子 `rename`，生成最终 `.md`。不执行额外的 `fsync`。任何失败都不得让最终路径出现半成品；
程序尽力清理临时文件，同时保留主错误与清理错误。最终目标已经存在时按记录写入失败
处理，不覆盖原文件。记录文件缺失只能说明记录关闭、任务未启动或记录失败之一，不能
证明模型请求没有发生。
