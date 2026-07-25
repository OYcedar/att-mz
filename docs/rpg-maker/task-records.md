# RPG Maker Standard 翻译任务记录现行规格

## 1. 目的、范围与开关

任务记录是面向人工与 Agent 排障的高级可读投影。它把一个 Standard TaskBlock 的最终
输入、全部逻辑尝试、模型业务输出、逐 ID 验收和数据库提交终态收敛到同一个 Markdown
文件。它不是逐次 HTTP 抓包，也不承担恢复、重放、验收或数据库状态判断。

该能力默认关闭，只由 Translate 消费：

```toml
[rpg_maker]
record_translation_tasks = false
```

`record_translation_tasks` 可省略，默认 `false`。它不属于某个 Client，不进入翻译
state、保存的运行方案或项目数据库。开启后只记录 RPG Maker Standard TaskBlock；
Translate Lua 不生成任务记录，因为核心不拥有 Lua 私有协议的逐 ID 验收与提交终态。

## 2. 路径、身份与生命周期

文件固定写入：

```text
<project-workspace>/task-records/<run-id>/task-000001.md
```

文件序号来自本轮 Standard 计划中的稳定 ordinal，不来自 HTTP 发送或响应完成顺序。
同一个 TaskBlock 的全部重试归入同一个文件。关闭记录、Standard 计划为零或没有启动任何
Standard 任务时，不建立空的 `task-records/<run-id>` 目录。既有外部文件不参与当前格式
识别、迁移或自动清理。

每个已经发出 `TaskStarted` 的任务最终恰有一个任务记录终态。合作取消后停止启动新任务；
已经启动的任务仍须交回顺序最终化边界并完成记录，尚未启动的任务不生成文件。任务记录
写入必须在文件系统运行根 shutdown 前结束。

## 3. Markdown 信息结构

参考中文呈现固定为：

~~~markdown
# 翻译任务 000030 · 部分完成

`任务 30/128` · `尝试 2 次` · `验收 14/17` · `写入 21 处`

- Run ID：`...`
- 开始时间：`...`
- 总耗时：`...`
- Endpoint：`...`
- Model：`...`

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
- 已接受：14 项，写入 21 个实际位置
- 未接受：
  - `15`：占位符不匹配
  - `16`：缺少模型输出
  - `17`：检测到源语言残留
- 协议诊断：模型额外返回了未知 ID `99`
~~~

`System`、`User` 与输入历史中的 `Assistant` 按原始消息顺序直接作为 Markdown 渲染；
不得把 messages 再包装成 JSON。固定角色标题使用 `System`、`User`、`Thinking`、
`Assistant`，其余标题、状态和诊断使用本次 UI locale。原始 Markdown 中的标题、列表、
引用、表格和代码块保持自身语义。

自定义参数是所选 Client 的实际结构化 JSON 数据，只在本节使用 JSON 围栏。Endpoint、
Model 和 parameters 直接来自本次实际选中的 Client，不从请求 JSON 反向解析。任务记录
不采集 HTTP Header、完整 Provider 外层 JSON 或非 200 原始 wire body。

## 4. Thinking 与 Assistant

Thinking 只认当前受信协议中恰好一个合法、非空、非嵌套的 `<why>...</why>` 信封。
合法时只渲染标签内部正文，信封标签本身不显示；没有 Thinking 时省略整节。空白、嵌套、
重复、顺序非法或其他无效信封不猜测拆分。

合法 Assistant JSON 不展示 JSON 外壳，只按模型原始条目顺序，以 `### ID <id>` 展开
业务值；字符串数组的成员按原顺序各占一段。重复、未知或其他非法 ID 仍完整呈现，并在
“最终结果”中给出协议诊断；缺失 ID 也在最终结果中明确列出。业务验收与任务记录共享
响应边界已经建立的一次性解析投影；renderer 不重新猜测 `<why>`，也不重新实现 ID、
Placeholder 或语言验收规则。

无效 JSON 或无效 Thinking 信封时，`Assistant` 节使用足以包住正文的动态 Markdown
围栏保留完整原始内容，不因正文自身含反引号而截断；同时只显示响应解析边界产生的唯一
解析错误及精确的一基行列。Thinking 和原始 Assistant 只进入任务记录旁路，不进入权威
`TranslationTaskOutcome`、译文数据库或状态指纹。

## 5. 尝试与最终状态

“请求过程”按逻辑 attempt 记录开始、耗时及结构化结果。成功项可以包含 finish reason、
token、Provider request/response ID；失败项可以包含 HTTP 状态、Provider code/type、
`Retry-After`、等待事实和本地化的类型化原因，不显示诊断 JSON 外壳。只有下一次模型
请求确实开始后，上一项才写
“等待后重试”；等待期间取消时写计划等待及取消，等待已经完成但下一次请求未开始时只写
等待完成，不得声称发生了重试。不得保存原始 Header、任意错误 wire body，也不得靠解析
错误显示文本补猜这些事实。

“总耗时”从该任务发出 `TaskStarted` 开始，延续到顺序最终化线完成验收、提交判断并构造
互斥终态；它不提前停在 Executor 返回时，也不把非权威 Markdown 文件写入耗时反算进业务
终态。

每个已启动任务最终收敛为下列互斥状态之一：

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
Translate Lua、运行方案保存或运行级收尾失败而改写任务终态；这些仍是运行级诊断。

## 6. 唯一敏感信息与精确替换

现行敏感信息闭集只有：**本次实际选中 LLM Client 的 API key 实际值**。Prompt、原文、
译文、自定义参数、Thinking、Assistant、Provider 正文和用户内容不因内容类别成为敏感
信息。

任务记录建立时使用同一个 API-key 替换器，递归处理 Endpoint query、自定义参数键和值、
System、User、输入历史 Assistant、Thinking、输出 Assistant、Provider 标识和任务诊断。
凡与 API key 实际值精确匹配的文本片段替换为：

```text
[REDACTED API KEY]
```

替换只作用于 key 本身，不删除所在字段、段落或正文。API key 配置字段本身完全不进入
任务文档。`Authorization` 不是第二类敏感信息；任务记录不采集 HTTP Header。诊断确需
说明认证时只保留字段名与认证方案，不显示承载的 key。

普通 CLI、JSONL 与 Debug 继续消费职责边界提供的稳定结构化投影，不遍历任意错误链、
复制大段正文或任意控制字符。这里的限制服务于职责、稳定 schema、可读体积和输出边界，
不表示 API key 之外的正文被重新定义为敏感信息。

## 7. 非权威写入语义

顺序最终化边界在真实验收和数据库提交判断完成后，为每个终态建立一份完整不可变文档，
并同步提交给启用的 sink。任务记录使用独立的终态观察文件根；实际 Markdown 渲染和文件
future 只有在 Standard、Translate Lua、必要业务根收尾与运行方案终态全部固定后才开始
轮询，随后并发写入并关闭观察根。记录慢写或故障处理不能阻塞后续数据库提交、Translate
Lua 或运行方案，也不能改变取消观察时点。记录建立、渲染、写入、清理或观察根关闭失败
必须在 stderr 明示最终路径、失败操作、主错误和存在时的清理错误，但不得改变翻译结果、
数据库、退出码、重试、后续任务或运行方案。

文件先在目标目录写入同目录临时文件，完成 `write_all + flush + close` 后，再以不覆盖
方式原子 rename 到最终 `.md`。不执行耐久同步。任何失败都不得让最终路径出现半成品；
程序尽力清理临时文件，同时保留主错误与清理错误。最终目标已经存在时按记录写入失败
处理，不覆盖原文件。记录文件缺失只能说明记录关闭、任务未启动或记录失败之一，不能
证明模型请求没有发生。
