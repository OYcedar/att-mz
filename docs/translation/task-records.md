# 模型任务记录现行规格

`[translation].record_translation_tasks = true` 时，Translate 为每个实际发出的 TaskBlock
建立一份可读 Markdown：

```text
<project>/task-records/<Translate-RunId>/task-000001.md
```

没有模型任务时不建立空目录。记录包含：

- 引擎、任务序号、尝试次数和最终结果；
- 模型实际使用的 system 与 user message；
- 请求过程、可用的 thinking 和 assistant 输出；
- 每个 ID 的验收结果；
- 已提交数量和 Partial、Unavailable 或失败原因。

记录不得包含 API key、Authorization 值或其他由
[Chat Completions 规格](../runtime/chat-completions.md#6-敏感信息闭集唯一权威)定义的
敏感值。

任务记录只是诊断材料，不是译文状态、重试队列或恢复依据。写入失败应产生用户可见诊断，
但不改变模型请求、数据库提交、命令结果或退出码。
