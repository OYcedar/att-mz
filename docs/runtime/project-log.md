# ATT 项目日志现行规格

项目工作区建立后，每次运行使用独立文件：

```text
<project>/logs/<run-id>.jsonl
```

日志没有配置分区，不共享活动文件、不轮转，也不按文件大小停止。更早发生的 CLI 或配置
错误只写 stderr。

## 1. 事件与终态

每条事件至少包含时间、RunId、稳定序号、闭集 code、引擎、项目、命令和类型化 payload。
业务代码不能提交任意自由文本 code。

独立 Lua 使用以下专用事件：

- `lua.script`：脚本身份与 SHA-256；
- `lua.print`：脚本一次显式 `print(...)` 的安全单行正文；
- `lua.summary`：数据库调用次数、changed rows、译文操作次数和打印行数。

ATT 不自动把 SQL、参数、查询结果、Lua 变量或游戏正文变成日志事件。脚本显式打印的
内容属于有意输出，会作为 `lua.print` 保存；脚本不应打印不需要保留的正文。

正常结束前，writer 先排空普通事件，再依次写性能计数、主错误与相关错误、唯一
`run.finished`，并完成 flush/sync。panic 在仍持有运行上下文的边界转换成安全诊断，
`run.finished.outcome` 为 `outcome_unknown`。

日志建立、写入或关闭失败会在 stderr 显示一次降级诊断，但不改变业务结果、数据库或退出
码。普通事件在缓冲压力下可以丢弃并计数；性能、失败和终态记录不能被普通事件挤掉。

## 2. 模型任务记录

`[translation].record_translation_tasks = true` 时，Translate 还可以建立：

```text
<project>/task-records/<run-id>/task-000001.md
```

它保存单个 MV/MZ 或 Generic TaskBlock 的可读消息、响应和逐 ID 验收。格式和故障处理见
[模型任务记录规格](../translation/task-records.md)。独立 Lua 没有模型任务，不生成该
记录。

JSONL 日志与任务记录都不是权威业务状态。缺失不能证明模型请求或数据库提交没有发生。

## 3. 失败与敏感信息

CLI 与 JSONL 使用同一份结构化安全诊断，保留：

- 错误 code、阶段、路径或对象；
- 具体原因与 OS、SQLite 或 HTTP 稳定代码；
- 状态是否改变、是否已经生效、终态是否未知；
- 操作建议与恢复位置；
- 主错误、相关错误与清理错误。

日志采用
[Chat Completions 规格的敏感信息闭集](chat-completions.md#6-敏感信息闭集唯一权威)，
不另建清单。普通日志不自动复制模型正文、Lua 执行中的任意值、SQL、参数、查询结果、
游戏文本或 panic payload；`lua.print` 只记录脚本显式提交并经过安全处理的正文。

数据库和目录发布 journal 各自拥有恢复语义；日志不能用于补写、回滚或重放业务操作。
