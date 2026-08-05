# 模型任务记录现行规格

`[translation].record_translation_tasks` 省略时默认是 `true`。只有操作者明确不需要可读
任务记录时才设为 `false`。开启时，Translate 为每个实际发出的 TaskBlock 建立一份可读
Markdown：

```text
<project>/task-records/<Translate-RunId>/task-000001.md
```

目录中的 `<Translate-RunId>` 与同次项目 JSONL 的 RunId 完全相同。没有模型任务时不建立
空目录。每份记录包含：

- 引擎、任务序号、尝试次数和最终结果；
- 模型实际使用的 system 与 user message；
- 请求过程、可读的 Thinking、逐 ID 译文和可用的 assistant 输出；
- 每次失败尝试的安全 `DiagnosticReport` 渲染，包括可用的 HTTP transport 阶段、状态、
  code/type 和经过处理的供应商 `error.message`；
- 每个 ID 的验收结果；
- 已提交数量和 Partial、Unavailable 或失败原因。

任务记录只为实际发出的含 ID TaskBlock 建立，不记录本轮完全没有 ID 的块。记录中的 user
message 是实际请求，必须保留该稳定 TaskBlock 的全部 Group 和 Unit；无编号的 Current、
复用、重复、非源语、完全保护和空文本仍省略 ID 并作为语境出现。Partial 后再次运行时，
已接受项以无 ID 的安全目标译文出现，待处理项在原块内重新从 `"0"` 编号，不能只记录
孤立的失败原文。

`thinking_output = true` 时，模型已经返回 `message.content` 的任务记录同时包含：

- `Thinking`：从有效响应投影出的可读翻译判断；
- 按 ID 展开的译文和验收诊断；
- `Raw Assistant`：模型本次实际返回的 `message.content`。

`Raw Assistant` 使用能够包住正文的动态 Markdown fence，只执行现行敏感信息闭集要求的
精确替换。它不是 HTTP body、Header、供应商完整响应，也不能称为未经处理的字节副本。
Assistant 使用合法 `json` 围栏时，围栏只是响应外层，不属于 JSON 修复；严格解析成功且
思考关闭时，它不会仅因存在围栏而增加 `Raw Assistant` 或修复记录。
响应经过 JSON 修复时，任务记录在解析结果之后增加 `JSON Repairs` 表格，按发生顺序保存
稳定修复 kind 及其相对于完整原始 Assistant 的一基行、列，不保存被删除、插入或替换的
正文片段。此时即使思考关闭，也显示 `Raw Assistant`；严格解析成功且思考关闭的响应仍不
额外显示它。无效或未处理的 assistant 正文继续按现有失败记录保留。

JSON 修复成功是响应解析事实，不是 Warn、Partial 或错误，不建立项目 JSONL diagnostic，
也不改变任务终态、提交、退出码或重试语义。修复后的重复、非法、未知、缺少、Placeholder
或其他逐 ID 问题继续使用现有验收和诊断。

人工或 agent 排查译文返回时，可以对照 System、User、Thinking、Raw Assistant、逐 ID
诊断和最终结果，确认模型实际返回的 JSON 结构、ID、原文回显、截断、转义与源语残留。
这是一项有效的诊断证据，不是权威业务状态；Raw Assistant 缺失只表示证据不足，不授权
重新请求、重放、验收或提交译文。

任务记录中的数字 ID 只属于本次请求，消息不携带 Generic 的 `group_id + unit_id` 或
MV/MZ 的 `owner + group_location + unit_role`。因此记录不能直接充当 Lua locator，也没有
保存通用的“逐失败原因到稳定 Unit”映射。需要人工或 agent 补译时，先按
[Lua 审查流程](../lua/README.md#4-完整审查与人工或-agent-修订)从当前数据库重新取得精确
locator 和完整 Unit 集合，再结合本记录的请求语境诊断。

记录中绝不写入 API key、Authorization 值或其他由
[Chat Completions 规格](../runtime/chat-completions.md#6-敏感信息闭集唯一权威)定义的
敏感值。任务记录不保存非 2xx 原始 body；供应商标准错误消息沿用同一闭集替换并清理为
单行文本后，才进入尝试原因。

任务记录只是诊断材料，不参与译文状态、重试队列或恢复判断。Partial、Unavailable 或
任务失败的具体问题由当前 RunId 的 `diagnostic.translation_task` 原子 occurrence 保存；
对应 `task.finished` 只引用 occurrence ID。每次 Translate 的完整任务计数和引擎专用汇总
由唯一 `translation.finished` 保存，不以该开关或 Markdown 是否写入成功为条件。

Ctrl-C 到达后，不再为尚未开始的模型任务建立记录；已经形成请求、响应或明确终态的记录
必须完成渲染和写入后，Translate 才关闭记录所需的执行资源。取消不能让已经形成的记录
无声消失。

渲染、建立目录、写入、flush、sync、临时文件清理或 worker 收尾失败时，ATT 建立包含
阶段、目标路径、稳定错误 code、类型化 issue、`effect` 与 `resolution` 的
`DiagnosticReport`。项目 JSONL 仍可写时，该报告作为 `diagnostic.task_record` Warn
occurrence 进入同一 RunId；主错误与 `cleanup`、`shutdown` 或 `observability` 相关报告在
同一原子事件内分别保存。该记录处理完成后，stderr 立即显示具体警告，而不是只显示
“任务记录不可用”。

上述任务记录故障不改变模型请求、数据库提交或业务结果。警告本身无法写入或完成呈现时，
属于独立的进程呈现失败，退出码为 `1`。
