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
- `performance.counters`。

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
  "relation": "primary",
  "object": "story.jsonl:line3:unit2:text",
  "reason": "译文没有保留原文中的 Placeholder",
  "impact": "此前确认的进度仍然保留；指出的内容没有完成",
  "help": "保留原文中的控制码和 Placeholder，并保持必要顺序"
}
```

`relation` 取 `primary`、`cleanup`、`rollback`、`discard`、`finalization`、`shutdown` 或
`observability`；`object` 使用自然路径、可读 ID、项目数据库或命令对象；`reason` 说明
直接原因；`impact` 说明业务状态是否未改、进度保留、已经生效、需要恢复或结果未知；
`help` 说明修改方法。五项都必须是非空、经过安全处理的单行文本。

诊断 payload 不保存 `report`、`effect`、`stage`、`issue`、`resolution`、内部状态
code、数据库行、owner、group location、unit role、编码位置、SQLite 查询 ID/code、原始
供应商请求 ID 或 expected/actual fingerprint。主错误与清理错误需要分别处理时，分别写成
各自可读的问题并保存实际 relation，不把递归内部报告倾倒给使用者。

终态事件只保存本次操作的结果和必要汇总，不引用诊断 ID，也不复制一份泛化错误。使用者
按同一 RunId 中相邻的 `diagnostic.*` 读取具体关系、对象、原因、影响和处理办法。

## 4. Translate 与任务汇总

每个实际开始的模型任务恰好写一条 `task.finished`。结果可以是 complete、partial、
unavailable、failed、not committed after an earlier failure 或 cancelled；没有开始的任务不
伪造完成事件。
任务只在第一次真实外部 HTTP attempt 开始时写 `task.started`。请求构造失败、准入前取消
或服务停发门拒绝仍是 not_started，不会产生伪造的 attempt、started 或 `task.finished`。
`task.finished.attempts` 必须大于零，并只计算该任务真实开始的 HTTP attempt。

每次 Translate 恰好写一条 `translation.finished`，保存完整任务计数和对应引擎的业务
汇总。计数满足：

```text
started = complete + partial + unavailable + failed + cancelled
planned = started + not_started
```

Generic 汇总保存 cleared、reused、accepted、written、conflicted Unit 与响应问题数；RPG
Maker 汇总保存接受、写入、剩余、协议和协调结果。Generic 另保存 planned_units 与
remaining_units；RPG Maker 分别保存 remaining_decisions 与 remaining_locations。两者都
保存 recoverable_request_exhaustions 和 request_admission_stopped。不同引擎不共用含义
不同的字段。

Generic 的 `planned_units - remaining_units` 是模型计划中已经成功写入的 Unit；
`written_units` 还可以包含既有译文复用写入，因此不能用 `written_units` 反推模型剩余量。
模型写入不得多于 accepted Unit，总写入减去模型写入不得多于本次复用目标。RPG Maker 的
accepted decisions 不能多于 written locations，remaining decisions 不能多于
remaining locations。NoWork 和 Complete 的剩余必须为零。

只要 Translate 已经形成计划事实，Complete、Incomplete、Failed 和 Cancelled 都保存当时
的 Task 计数和引擎汇总。Failed 或 Cancelled 不得把已经开始、未开始、已写入和剩余工作
清零；终端短汇总直接使用同一份事实。Generic 的 remaining_units 是计划交给模型的 Unit
减去实际写入的 Unit，CAS 冲突不算写入；RPG Maker 的剩余决策和位置同样按实际提交递减。
服务停发后没有开始的 Task 只计入 not_started。
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

所有事件进入一个 FIFO 和单 writer。必要事件直接进入；普通阶段和性能事件使用既有在途
窗口，压力满时只累计实际丢失数量，不阻塞业务线程，也不限制项目事件总量。终端整数百分比
只用于实时呈现，不逐行写入项目 JSONL；项目日志仍保存阶段开始、完成、停止和最终结果。

日志建立、序列化、写入、flush、sync、channel 或 writer 故障分别累计。同一故障键只写一条
`diagnostic.project_log`，累计次数进入该诊断的具体原因；不得另写只有故障种类或数量的模糊
降级摘要。第一条 write 失败后停止继续建立 JSON，排空队列并统计未持久化事件；writer 已经
不可用时，无法写入自身日志的故障仍通过同一类型化事实呈现到 stderr。writer 健康时，正常关闭
依次完成：

1. 关闭生产者并排空已接收事件；
2. 按自然顺序逐项写日志故障诊断；
3. 写性能计数；
4. 写尚未持久化的终端诊断；
5. 写唯一且最后的 `run.finished`；
6. flush；
7. sync。

日志无法建立时不要求日志记录自身失败，stderr 必须显示对象、原因、影响和处理办法。日志故障不
改变业务结果、数据库状态或业务退出语义；只有警告或终态本身无法通过 stdout/stderr、
worker 或 channel 呈现时，才成为独立进程呈现失败。

最终业务正文与诊断的首次 stdout/stderr 写入和 flush 发生在项目日志关闭前；当时已经确认的
呈现失败作为本次终态诊断保存。项目日志关闭后，只允许把尚未确认的完整正文、该失败诊断和
最终日志警告作为一个批次向健康的相反流回退一次；回退本身失败不再重入日志收尾或派生无限
诊断。

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
[OpenAI-compatible HTTP 规格](openai-compatible.md#6-敏感信息闭集唯一权威)。API key、
Authorization、SQL、参数、查询结果、游戏正文、非 2xx 原始 body 和 panic payload 不进入
项目日志。Lua 明确 `print` 的内容由脚本作者负责，并仍经过单行安全处理。

恢复语义由数据库和目录发布 journal 各自掌管；项目日志只记录已经确认的事实，不参与
补写、回滚、重放或副作用准入。
