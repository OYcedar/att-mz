# ATT 项目日志现行规格

Init、Extract、Translate、WriteBack 和 Lua 在项目工作区建立后，每次运行使用一份独立日志：

```text
<project>/logs/run-000001.jsonl
```

RunId 是项目内自然递增序号。ATT 同时扫描既有日志与任务记录目录，并用 `create_new` 原子
保留下一个日志文件；冲突时递增。文件名不使用 UUID、hash 或数据库键。发生在日志建立前
的 CLI、配置和 Manual 错误只写 stderr。

## 1. 记录格式

每行是一个完整 JSON 对象，顶层字段固定为：

```text
timestamp, sequence, run_id, level, event, context, payload, message
```

- `sequence` 在同一 RunId 内从 1 单调递增；
- `context` 包含 `locale`、`engine`、`project` 和 `command`；
- `event` 是自然事件名，例如 `run.started`、`phase.completed` 或 `lua.print`；
- `payload` 只保存该事件确实需要的结构化事实；
- `message` 是按本次 UI 语言生成的可读单行说明；
- 未知字段和不一致的 event、level、payload 组合无效。

日志不使用公开 `code` 或诊断引用字段。内部实现可用类型化错误和临时身份维持控制流，
但这些内容不进入 JSONL。

## 2. 事件

项目事件覆盖：

- `run.started`、`run.cancel_requested`、`run.finished`；
- `phase.started`、`phase.completed`、`phase.stopped`；
- `run_plan.resolved`、`run_plan.finalized`；
- `task.started`、`task.finished`、`translation.finished`、`retry.summary`；
- `publication.started`、`publication.finished`；
- `lua.print`；
- `diagnostic.*`；
- `observability.project_log_degraded`、`performance.counters`。

普通事件只描述已经确认的事实。阶段实际完成后才能写 `phase.completed`；失败或取消使用
`phase.stopped`。首个取消信号只写一次，最终结果只由唯一且最后的 `run.finished` 表达。

Lua 日志不保存脚本摘要或无实际作用的调用汇总。SQL、参数、查询结果、Lua 变量和游戏正文
不会自动进入日志；只有脚本显式 `print(...)` 的安全单行正文写成 `lua.print`。

## 3. 诊断事件

诊断事件按问题所属范围使用：

```text
diagnostic.run
diagnostic.run_plan
diagnostic.translation_task
diagnostic.extract
diagnostic.write_back
diagnostic.publication
diagnostic.task_record
diagnostic.project_log
```

公开 payload 固定为：

```json
{
  "object": "story.jsonl:line3:unit2:text",
  "reason": "译文没有保留原文中的 Placeholder",
  "help": "保留原文中的控制码和 Placeholder，并保持必要顺序"
}
```

`object` 使用自然路径、可读 ID、项目数据库或命令对象；`reason` 说明直接原因；`help` 说明
修改方法。三项都必须是非空、经过安全处理的单行文本。

诊断 payload 不保存 `report`、`effect`、`stage`、`issue`、`resolution`、relation、内部状态
code、数据库行、owner、group location、unit role、编码位置、SQLite 查询 ID/code、原始
供应商请求 ID 或 expected/actual fingerprint。主错误与清理错误需要分别处理时，分别写成
各自可读的问题，不把递归内部报告倾倒给使用者。

终态事件只保存本次操作的结果和必要汇总，不引用诊断 ID，也不复制一份泛化错误。使用者
按同一 RunId 中相邻的 `diagnostic.*` 读取具体对象、原因和修改方法。

## 4. Translate 与任务汇总

每个实际开始的模型任务恰好写一条 `task.finished`。结果可以是 complete、partial、
unavailable、failed、not committed after an earlier failure 或 cancelled；没有开始的任务不
伪造完成事件。

每次 Translate 恰好写一条 `translation.finished`，保存完整任务计数和对应引擎的业务
汇总。计数满足：

```text
started = complete + partial + unavailable + failed + cancelled
planned = started + not_started
```

Generic 汇总保存 cleared、reused、accepted、written、conflicted Unit 与响应问题数；RPG
Maker 汇总保存接受、写入、剩余、协议和协调结果。不同引擎不共用含义不同的字段。
Placeholder 等规划错误发生在模型请求前时，日志写可读 `diagnostic.run_plan`，不得声明
Task 已开始。

任务记录中的数字 ID 只属于一次模型请求，不能用于 Manual、Lua 或数据库定位。人工补译
使用 [Manual](../manual/README.md) 或高级 Lua 返回的可读 ID。

## 5. RunPlan、发布与运行结果

`run_plan.resolved` 保存 Init、Extract 或 Translate 本次实际采用的选择，并说明来自显式
参数、项目状态还是产品默认。`run_plan.finalized` 只说明运行方案是否保存、数据库是否有
明确提交结果以及命令是否继续，不附内部诊断引用。

`publication.started` 保存输出根。`publication.finished` 保存发布、未发布、需要恢复或结果
未知，以及成功时的引擎专用汇总。恢复路径只在确实需要使用者处理时，以自然路径出现在
对应可读诊断中。

`run.finished` 只表达成功、取消、失败、需要恢复或结果未知。需要恢复表示当前结果明确，
但现场必须保留或下一次同目标命令需要先恢复；结果未知只用于是否生效确实无法确认的情况。
两者不能互相替代，也不能伪造成已经回滚。

panic 在仍持有运行上下文的边界转换为安全诊断，不保存 panic payload，不伪造阶段完成。

## 6. 写入、降级与关闭

所有事件进入一个 FIFO 和单 writer。必要事件直接进入；高频进度事件使用固定在途窗口，
压力满时只累计实际丢失数量，不阻塞业务线程，也不限制项目事件总量。

日志建立、序列化、写入、flush、sync、channel 或 writer 故障分别累计。第一条 write 失败
后停止继续建立 JSON，排空队列并统计未持久化事件。writer 健康时，正常关闭依次完成：

1. 关闭生产者并排空已接收事件；
2. 写日志降级汇总；
3. 写性能计数；
4. 写尚未持久化的终端诊断；
5. 写唯一且最后的 `run.finished`；
6. flush；
7. sync。

日志无法建立时不要求日志记录自身失败，stderr 必须显示对象、原因和修改方法。日志故障不
改变业务结果、数据库状态或业务退出语义；只有警告或终态本身无法通过 stdout/stderr、
worker 或 channel 呈现时，才成为独立进程呈现失败。

## 7. 模型任务记录

`[translation].record_translation_tasks` 省略时默认 `true`。Translate 为每个实际发出的
TaskBlock 建立：

```text
<project>/task-records/run-000001/task-000001.md
```

没有模型任务时不建立空目录。任务记录使用目标文件派生的固定临时名完成原子替换，不使用
随机后缀。格式和故障处理见[模型任务记录规格](../translation/task-records.md)。Lua 和
Manual 不生成模型任务记录。

JSONL 和 Markdown 都只是事后证据，不参与译文状态、提交、恢复或重放。任务记录写入故障
不重发模型请求、不回滚已确认译文，也不改变业务结果；警告无法呈现时才返回 `1`。

## 8. 敏感信息

敏感信息唯一权威是
[Chat Completions 规格](chat-completions.md#6-敏感信息闭集唯一权威)。API key、
Authorization、SQL、参数、查询结果、游戏正文、非 2xx 原始 body 和 panic payload 不进入
项目日志。Lua 明确 `print` 的内容由脚本作者负责，并仍经过单行安全处理。

恢复语义由数据库和目录发布 journal 各自掌管；项目日志只记录已经确认的事实，不参与
补写、回滚、重放或副作用准入。
