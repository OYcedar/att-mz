# 模型任务记录现行规格

`[translation].record_translation_tasks = true` 时，Translate 为每个实际发出的 TaskBlock
建立一份可读 Markdown：

```text
<project>/task-records/<Translate-RunId>/task-000001.md
```

没有模型任务时不建立空目录。每份记录包含：

- 引擎、任务序号、尝试次数和最终结果；
- 模型实际使用的 system 与 user message；
- 请求过程、可用的 thinking 和 assistant 输出；
- 每次失败尝试的结构化原因，包括可用的 HTTP code/type 和经过处理的供应商
  `error.message`；
- 每个 ID 的验收结果；
- 已提交数量和 Partial、Unavailable 或失败原因。

记录中绝不写入 API key、Authorization 值或其他由
[Chat Completions 规格](../runtime/chat-completions.md#6-敏感信息闭集唯一权威)定义的
敏感值。任务记录不保存非 2xx 原始 body；供应商标准错误消息沿用同一闭集替换并清理为
单行文本后，才进入尝试原因。

任务记录只是诊断材料，不参与译文状态、重试队列或恢复判断。写入失败会产生用户可见
诊断，但模型请求、数据库提交、命令结果和退出码都不受影响。
