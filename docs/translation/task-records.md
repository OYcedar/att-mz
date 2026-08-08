# 模型任务记录现行规格

`[translation].record_translation_tasks` 省略时默认 `true`。只有操作者明确不需要可读任务
记录时才设为 `false`。开启时，Translate 为每个实际发出的 TaskBlock 建立一份 Markdown：

```text
<project>/task-records/run-000001/task-000001.md
```

目录 RunId 与同次项目日志相同。任务文件按自然序号排列，使用 `.task-000001.md.tmp` 这类
目标派生临时名原子替换，不使用 UUID、hash 或随机后缀。没有模型任务时不建立空目录。

## 1. 记录内容

每份记录包含：

- 引擎、任务自然序号、尝试次数和最终结果；
- 模型实际使用的 system 与 user message；
- 请求过程、可读 Thinking、逐临时 ID 译文和可用的 assistant 输出；
- 每次失败尝试的安全说明，包括可用的 HTTP 阶段、状态和经过处理的供应商错误消息；
- 每个临时 ID 的验收结果；
- 已提交数量和 Partial、Unavailable 或失败原因。

任务记录只为实际发出的含临时 ID TaskBlock 建立，不记录完全没有待处理项的块。user message
是实际请求，保留稳定 TaskBlock 的全部 Group 和 Unit；Current、复用、重复、非源语、完全
保护和空文本仍作为无 ID 语境出现。

Partial 后再次运行时，已接受项以无 ID 的安全目标译文出现，待处理项在原块内重新从 `0`
编号。ATT 不只记录或发送孤立的失败原文。

## 2. Thinking、Raw Assistant 与 JSON 修复

`thinking_output = true` 时，模型已经返回 `message.content` 的任务记录同时包含：

- `Thinking`：从有效响应投影出的可读翻译判断；
- 按临时 ID 展开的译文和验收说明；
- `Raw Assistant`：模型本次实际返回的 `message.content`。

`Raw Assistant` 使用能够包住正文的动态 Markdown fence，并执行现行敏感信息闭集要求的
精确替换。它不是 HTTP body、Header 或供应商完整响应，也不称为未经处理的字节副本。

响应经过公共保守 JSON 修复时，记录在解析结果后增加 `JSON Repairs` 表格，按发生顺序
保存修复种类及相对于完整原始 Assistant 的一基行、列，不保存被删除、插入或替换的正文
片段。严格解析成功且思考关闭时，不额外增加 Raw Assistant 或修复记录。

JSON 修复成功只是解析事实，不是 Warn、Partial 或错误，不改变任务终态、提交和退出码。
修复后的重复、非法、未知、缺少、Placeholder 或其他逐 ID 问题继续使用正常验收。

## 3. 临时 ID 与人工补译

记录中的数字 ID 只属于本次模型请求，下一次运行可以重新编号。它不是项目位置、数据库键
或 Manual ID，不能传给 Lua 或数据库。

需要人工或 agent 补译时：

1. 运行对应项目的 `manual export`，取得当前可读 ID 和原文；
2. 对含义不明的条目，把全部 ID 合并到一次 `ctx.translation.context(ids)`；
3. 结合本记录中的完整请求语境、当前术语和 Lua 返回的 Group 上下文填写 TOML；
4. 运行 `manual check` 和 `manual apply`。

不要从重复原文、任务序号或任务记录文件名猜数据库位置。普通补译不读取 raw schema，也不
需要内部位置字段。完整流程见 [Manual](../manual/README.md)和
[Lua](../lua/README.md)。

## 4. 敏感信息

任务记录不写 API key、Authorization 或其他由
[Chat Completions 规格](../runtime/chat-completions.md#6-敏感信息闭集唯一权威)定义的
敏感值。它不保存非 2xx 原始 body；供应商标准错误消息经过闭集替换并清理为单行文本后，
才进入尝试原因。

任务记录是诊断证据，不是权威业务状态。Raw Assistant 缺失只表示证据不足，不授权重新
请求、重放、验收或提交译文。完整任务计数和引擎汇总仍由同次项目日志中的
`translation.finished` 保存，不以 Markdown 是否写入成功为条件。

## 5. 取消与写入故障

Ctrl-C 到达后，不再为尚未开始的模型任务建立记录；已经形成请求、响应或明确终态的记录
必须完成渲染和写入后，Translate 才关闭相关执行资源。

渲染、建目录、写入、flush、sync、临时文件清理或 worker 收尾失败时，stderr 和仍可用的
项目日志只说明失败文件、直接原因和修改方法。公开诊断不保存内部阶段、错误 code、递归
报告、数据库位置或供应商请求 ID。

任务记录故障不重发模型请求、不回滚已经确认的译文，也不改变业务结果。警告本身无法写入
或完成呈现时，属于独立进程呈现失败，退出码为 `1`。
