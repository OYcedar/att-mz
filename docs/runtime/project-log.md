# ATT 项目日志现行规格

项目工作区建立后，每次运行使用独立文件：

```text
<project>/logs/<run-id>.jsonl
```

一次运行一份文件：不分区、不共享活动文件、不轮转，也不按文件大小停止。发生在
日志建立之前的 CLI 或配置错误只写 stderr。

## 1. 记录格式与事件闭集

每行是一个完整 JSON 对象，顶层字段固定为：

```text
timestamp, sequence, run_id, level, code, context, payload, message
```

- `sequence` 在同一 RunId 内单调递增；
- `context` 必须包含 `locale`、`engine`、`project` 和 `command`；
- Profile、术语和 Placeholder 等运行计划事实只写入类型化 `run_plan.resolved`，不放进
  可空上下文字段；
- `level`、`code`、`payload` 形状和本地化 `message` 全部由封闭事件类型决定，调用方不能
  自由组合；
- wire 类型拒绝未知字段和不一致的 code、level、payload 组合。

项目事件闭集包括 Run、取消、Phase、RunPlan、Translate Task、Translate 汇总、重试、
Publication、Lua、诊断、日志降级、性能计数和唯一运行终态。普通事件只描述已经确认的
事实：阶段只有显式完成后才写 `phase.completed`；失败或取消使用 `phase.stopped`，进入下一
阶段、普通收尾和 Drop 都不能推断上一阶段完成。首个取消信号只写一次
`run.cancel_requested`，最终结果只由 `run.finished` 表达。

独立 Lua 使用：

- `lua.script`：脚本身份与 SHA-256；
- `lua.print`：脚本一次显式 `print(...)` 的安全单行正文；
- `lua.summary`：数据库调用次数、changed rows、译文操作次数和打印行数；每份已经建立
  的 Lua 项目日志恰好写一条，失败或回滚时也记录已经发生的操作，但不表示事务已提交。

SQL、参数、查询结果、Lua 变量和游戏正文不会自动变成日志事件；只有脚本有意提交的
`print` 内容会作为 `lua.print` 保存。

## 2. 原子诊断与引用

每个独立问题形成一条原子诊断事件。事件 code 由 scope 唯一决定：

- `diagnostic.run`
- `diagnostic.run_plan`
- `diagnostic.translation_task`
- `diagnostic.extract`
- `diagnostic.write_back`
- `diagnostic.publication`
- `diagnostic.task_record`
- `diagnostic.project_log`

payload 固定为：

```text
id, scope, report
```

`id` 是当前 RunId 内单调递增的非零 occurrence ID，只用于同一日志中的引用，不进入项目
数据库或业务状态。`report` 固定包含：

```text
effect, primary, related
```

`primary` 包含由具体 issue 唯一推导的 `code`、`stage`、`issue` 和 `resolution`；调用方不能
另传泛化原因、动作或恢复组件。`related` 中每项包含 `relation` 与递归 `report`，relation
只允许 `cleanup`、`rollback`、`discard`、`finalization`、`shutdown` 或
`observability`。主错误及其全部相关错误在同一行内，不会被并发事件插入。

`effect` 只允许：

- `unchanged`
- `progress_preserved`
- `applied`
- `applied_run_plan_not_saved`
- `applied_finalization_failed`
- `recovery_required`
- `outcome_unknown`

具体对象、路径、规则号、Unit locator、HTTP 阶段、SQLite code、I/O operation 与 OS code
保存在各 issue 的类型化字段内。恢复位置同样属于具体 issue，例如目录发布的
`output_root`、`candidate_root`、`residual_path` 或 `recovery_artifacts`；不能用自由字符串
字段袋代替这些事实。

Rules command 的非字符串跳过与 WriteBack 人工布局要求都是相应 scope 的具体 Warn 诊断。
前者保留规则来源、规则号、命令 code、参数位置、实际类型和聚合计数；后者保留精确
`group_location`、`role`、`region` 与 `max_fullwidth_chars`。它们不能退化为只有计数的
自由文本事件。

`phase.stopped`、`task.finished`、`run_plan.finalized`、`translation.finished`、
`publication.finished` 和 `run.finished` 只引用已经出现的 occurrence ID，不复制或重新解释
诊断。导致运行失败的 Task 或 Publication occurrence 直接由终态复用。

## 3. Translate、RunPlan、Publication 与运行终态

每个实际开始的模型任务恰好写一条 `task.finished`，终态为 `complete`、`partial`、
`unavailable`、`failed`、`not_committed_after_earlier_failure` 或 `cancelled`；`partial`、
`unavailable` 和 `failed` 引用 `diagnostic.translation_task` occurrence。后者只在该任务已经
得到可提交结果、但前序任务失败而没有应用时使用，并复用那个前序失败 occurrence，仍计入
Translate 汇总的 `failed`。没有开始的任务不伪造 `task.finished`，只计入 Translate 汇总的
`not_started`。

每次 Translate 命令恰好写一条 Required `translation.finished`，结果为：

- `not_started`
- `no_work`
- `complete`
- `incomplete`
- `failed`
- `cancelled`

除 `not_started` 外，任务计数固定包含 `planned`、`started`、`complete`、`partial`、
`unavailable`、`failed`、`cancelled` 和 `not_started`，并满足：

```text
started = complete + partial + unavailable + failed + cancelled
planned = started + not_started
```

Generic 汇总保存 cleared/reused/accepted/written/conflicted units 和 response problems；
RPG Maker 汇总保存 decision、location、protocol、request exhaustion 与 reconciliation 的
引擎专用计数。两种引擎不共用含义不同的字段。Markdown 任务记录关闭或写入失败都不会
删除这些 JSONL 事实。

`run_plan.resolved` 使用 Init、Extract、Translate 三种类型化 plan，并说明每个值来自显式
参数、项目状态还是产品默认。`run_plan.finalized` 保存数据库路径、事务状态、运行是否继续，
以及 `saved`、`not_saved`、`saved_finalization_failed` 或 `outcome_unknown`；非成功结果必须
引用诊断 occurrence。

`publication.started` 只保存 output root。`publication.finished` 使用：

- `published`：同时保存引擎专用汇总；
- `not_published`
- `recovery_required`
- `outcome_unknown`

后三种结果必须引用 `diagnostic.publication` occurrence。Generic 汇总保存 files、translated
units 和 retained source units；RPG Maker 汇总保存 translated/original/auto-wrapped
units、插入换行、全角缩进与 manual-layout units。

`run.finished.payload.result` 只允许：

- `succeeded`
- `cancelled`
- `failed { diagnostic }`
- `recovery_required { diagnostic }`
- `outcome_unknown { diagnostic }`

后三种不能在没有主 occurrence ID 时构造。`recovery_required` 与 `outcome_unknown` 分别
表示“状态明确但必须保留或处理恢复现场”和“是否生效确实无法确认”，不能互相替代。
panic 在仍持有运行上下文的边界转换成预登记的安全诊断；不保存 panic payload，也不伪造
阶段完成。

## 4. 写入、降级与关闭

所有事件进入一个 FIFO 和单 writer。Required 事件直接进入；BestEffort 事件使用固定的
8192 个在途 permit，压力满时按事件 code 计数丢弃，不阻塞异步业务线程。BestEffort 仅有
`phase.started`、`phase.completed`、`task.started`、`retry.summary` 与 `lua.print`；其余
生命周期、诊断、汇总、性能和终态事件均为 Required。

日志建立、序列化、写入、flush、sync、channel 或 writer 故障按类型化键分别累计次数。尽力
事件的 8192 个 permit 用尽时，记录 `best_effort_backpressure`（包含被丢弃的事件 code），不是
`channel_closed`。每项健康计数在 wire 中要么是 `exact { count }`，要么在 `u64` 溢出后是
`at_least { minimum }`；后者只声明可证明的下界，绝不把下界当成精确次数。

第一条 write 失败后停止继续拼接 JSON，排空 channel 并统计未持久化事件；stderr 使用消费
游标及时显示每种新故障。writer 仍健康时，正常关闭依次完成：

1. 关闭生产者并排空已经接收的事件；
2. 写 `observability.project_log_degraded`；
3. 写 `performance.counters`；
4. 写尚未持久化的终端诊断；
5. 写唯一且最后一条 `run.finished`；
6. flush；
7. sync。

日志无法建立时不要求日志记录自身失败，stderr 必须显示同一份完整结构化诊断，业务继续按
真实结果执行。项目日志故障不改变业务结果、数据库状态或业务退出语义；只有警告本身无法
通过 stdout/stderr、worker 或 channel 呈现时，才成为独立进程呈现失败并返回 `1`。

普通 `finish()` 的生命周期校验失败时，调用者收到对应的 `FinishError`。随后 runtime 的 Drop
若仍能写终态，只能把这份真实的项目日志合同诊断提升为 `outcome_unknown`；不得改用之前为
panic 预登记的诊断，也不得伪造阶段完成。

## 5. 模型任务记录

`[translation].record_translation_tasks` 省略时默认是 `true`；只有显式设为 `false` 才
关闭 Markdown 任务记录。开启时，Translate 为每个实际发出的 TaskBlock 建立：

```text
<project>/task-records/<run-id>/task-000001.md
```

没有实际模型任务时不建立空目录。格式和故障处理见
[模型任务记录规格](../translation/task-records.md)。独立 Lua 没有模型任务，因而不生成
该记录。

任务记录的渲染、建立目录、写入、flush、sync、临时文件清理或 worker 收尾失败时，建立
`diagnostic.task_record` occurrence；主错误与 `cleanup`、`shutdown` 或
`observability` 相关报告在同一 occurrence 内保留。stderr 同时显示具体降级警告。该故障
不重发模型请求、不回滚已经确认的译文，也不改变业务结果；警告无法呈现时才返回 `1`。

JSONL 日志和任务记录都是事后证据，不是权威业务状态：它们缺失，说明不了模型请求或
数据库提交有没有发生。任务记录中的临时 ID 也不是项目数据库 locator。

## 6. 敏感信息与恢复权威

CLI、JSONL 和 Markdown 任务记录消费同一份安全 `DiagnosticReport`，不从 `Display`、
本地化阶段名或拼接字符串反推事实。动态正文只允许安全 OS 信息、经过闭集替换与单行清理
的供应商消息，以及 Lua 显式 `print`。

敏感信息唯一权威是
[Chat Completions 规格的敏感信息闭集](chat-completions.md#6-敏感信息闭集唯一权威)。
API key、Authorization、SQL、参数、查询结果、游戏文本、非 2xx 原始 body 和 panic payload
不会进入项目日志。

恢复语义由数据库和目录发布 journal 各自掌管；日志只记录诊断与引用，不参与补写、回滚、
重放或副作用准入。
