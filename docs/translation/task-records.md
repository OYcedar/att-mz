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
- 请求过程、可用的 thinking 和 assistant 输出；
- 每次失败尝试的结构化原因，包括可用的 HTTP code/type 和经过处理的供应商
  `error.message`；
- 每个 ID 的验收结果；
- 已提交数量和 Partial、Unavailable 或失败原因。

任务记录只为实际发出的含 ID TaskBlock 建立，不记录本轮完全没有 ID 的块。记录中的 user
message 是实际请求，必须保留该稳定 TaskBlock 的全部 Group 和 Unit；无编号的 Current、
复用、重复、非源语、完全保护和空文本仍作为语境出现。Partial 后再次运行时，已接受项以
`[-]` 目标译文出现，待处理项重新从 `[1]` 编号，不能只记录孤立的失败原文。

记录中绝不写入 API key、Authorization 值或其他由
[Chat Completions 规格](../runtime/chat-completions.md#6-敏感信息闭集唯一权威)定义的
敏感值。任务记录不保存非 2xx 原始 body；供应商标准错误消息沿用同一闭集替换并清理为
单行文本后，才进入尝试原因。

任务记录只是诊断材料，不参与译文状态、重试队列或恢复判断。Partial、Unavailable、
任务失败和逐任务诊断始终由当前 RunId 的 `task.finished`、`task.diagnostic` 与
`result.partial` JSONL 事件记录，不以该开关或 Markdown 是否写入成功为条件。

Ctrl-C 到达后，不再为尚未开始的模型任务建立记录；已经形成请求、响应或明确终态的记录
必须完成渲染和写入后，Translate 才关闭记录所需的执行资源。取消不能让已经形成的记录
无声消失。

渲染、建立目录、写入、flush、sync、临时文件清理或 worker 收尾失败时，ATT 建立包含
阶段、目标路径、稳定错误 code、具体原因和处理办法的结构化诊断。项目 JSONL 仍可写时，
该诊断以 `observability.task_record_failed` Warn 事件进入同一 RunId；主错误与相关清理
错误分别保存。该记录处理完成后，stderr 立即显示具体警告，而不是只显示“任务记录
不可用”。

上述任务记录故障不改变模型请求、数据库提交或业务结果。警告本身无法写入或完成呈现时，
属于独立的进程呈现失败，退出码为 `1`。
