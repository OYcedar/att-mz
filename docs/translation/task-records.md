# 模型任务记录现行规格

`[translation].record_translation_tasks` 省略时默认 `true`。开启时，Translate 为每个实际
发出的模型任务建立一份 Markdown：

```text
<project>/task-records/run-000001/task-000001.md
```

目录 RunId 与同次项目日志相同，任务文件按自然序号排列。写入使用目标文件派生的临时名
原子替换，不使用 UUID、hash 或随机后缀；没有模型任务时不建立空目录。

## 1. 最小内容

每份记录只保存三类事实：

- 实际发给模型的 User message；
- 已经收到时，模型返回的原始 Assistant 正文；
- 最终状态、已验收和已确认写入的数量，以及使用者需要处理的最终诊断。

User message 按实际请求保存，不另造简化版。Assistant 使用能够包住正文的动态 `text`
fence，只执行敏感信息替换，不解析后重新排版，也不另行展开 `think`、原文回显、逐 ID
译文或 JSON 修复过程。

任务记录不重复保存 System message、引擎名、RunId、时间、耗时、Endpoint、模型、请求参数、
尝试过程、token 统计、供应商请求标识或 JSON 修复坐标。这些内容不参与人工补译、验收、
提交或恢复；确有独立诊断用途的运行事实由项目日志负责。

任务记录只为实际发出的含临时 ID 任务建立。Partial 后再次运行时，记录仍保存第二次实际
发送的完整 User message，不把失败原文单独改造成另一种请求。

## 2. 临时 ID 与人工补译

记录中的数字 ID 只属于本次模型请求，下一次运行可以重新编号。它不是项目位置、数据库键
或 Manual ID，不能传给 Lua 或数据库。

需要人工或 agent 补译时：

1. 运行对应项目的 `manual export`，取得当前可读 ID 和原文；
2. 对含义不明的条目，把全部 ID 合并到一次 `ctx.translation.context(ids)`；
3. 结合任务记录中的实际请求和 Assistant、当前术语及 Lua 返回的 Group 上下文填写 TOML；
4. 运行 `manual check` 和 `manual apply`。

不要从重复原文、任务序号或任务记录文件名猜数据库位置。完整流程见
[Manual](../manual/README.md)和[Lua](../lua/README.md)。

## 3. 敏感信息与权威性

任务记录不写 API key、Authorization 或其他由
[Chat Completions 规格](../runtime/chat-completions.md#6-敏感信息闭集唯一权威)定义的
敏感值。User 与 Assistant 中出现这些值时，只替换对应正文，不改写其余内容。

任务记录是旁路证据，不是权威业务状态。Assistant 缺失只表示没有这份证据，不授权重新
请求、重放、验收或提交译文；任务计数和引擎汇总仍由同次项目日志保存。

## 4. 取消与写入故障

Ctrl-C 到达后，不再为尚未开始的模型任务建立记录；已经形成请求、响应或明确终态的记录
在 Translate 关闭相关执行资源前完成写入。

渲染、建目录、写入、flush、sync、临时文件清理或 worker 收尾失败时，stderr 和仍可用的
项目日志只说明失败文件、直接原因和修改方法。任务记录故障不重发模型请求、不回滚已经
确认的译文，也不改变业务结果；警告本身无法呈现时属于独立进程呈现失败，退出码为 `1`。
