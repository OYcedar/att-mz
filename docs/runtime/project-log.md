# ATT 项目日志现行规格

项目工作区建立后，每次运行使用独立文件：

```text
<project>/logs/<run-id>.jsonl
```

一次运行一份文件：不分区、不共享活动文件、不轮转，也不按文件大小停止。发生在
日志建立之前的 CLI 或配置错误只写 stderr。

## 1. 事件与终态

每条事件都带着时间、RunId、稳定序号、闭集 code、引擎、项目、命令和类型化
payload；code 来自封闭集合，业务代码无法提交任意自由文本。

独立 Lua 使用以下专用事件：

- `lua.script`：脚本身份与 SHA-256；
- `lua.print`：脚本一次显式 `print(...)` 的安全单行正文；
- `lua.summary`：数据库调用次数、changed rows、译文操作次数和打印行数；每份已经建立
  的 Lua 项目日志恰好写一条，失败或回滚时也记录已经发生的操作，但不表示事务已提交，
  事务终态只由 `run.finished` 表达。

Rules command 直接参数出现可跳过的非字符串时，Extract 成功后写 Warn 事件
`extract.rules.command_non_string_skipped`。payload kind 是
`rules_command_non_string_skipped`，只保存 `rule_number`、`source_file`、
`command_code`、`parameter`、`actual_type` 和 `skipped_count`；不保存原始参数值或
逐命令位置。相同键已经聚合，事件按键稳定排序。

SQL、参数、查询结果、Lua 变量和游戏正文不会自动变成日志事件；只有脚本显式
`print` 的内容——有意输出——会作为 `lua.print` 保存。脚本只打印需要保留的
内容就好。

正常结束前，writer 先排空普通事件，再依次写性能计数、主错误与相关错误、唯一
`run.finished`，并完成 flush/sync。`run.finished.outcome` 只使用以下终态：

- `succeeded`：业务得到明确成功结果；
- `failed`：主运行明确失败，未被投影为下面的专用终态；仍要读取主诊断和全部相关诊断的
  `impact` 与 `recovery`，其中个别诊断仍可能要求保留恢复现场；
- `cancelled`：合作取消完成；
- `recovery_required`：业务状态已经明确，但操作者必须按诊断保留或处理恢复现场；
- `outcome_unknown`：提交、发布或进程异常使最终状态确实无法确认。

`recovery_required` 不能归入 `outcome_unknown`；已知需要恢复和无法判断是否生效是两种
不同事实。panic 在仍持有运行上下文的边界转换成安全诊断，只有无法证明业务终态时才把
`run.finished.outcome` 写成 `outcome_unknown`。

Translate 的任务事实不依赖 Markdown 任务记录开关。每个实际任务使用
`task.finished` 保存 `complete`、`partial`、`unavailable` 或 `failed`；存在具体原因时，
另写 `task.diagnostic`。本轮存在 Partial 或 Unavailable 时，`result.partial` 保存完整汇总。
关闭 Markdown 任务记录不会删除这些 JSONL 事件。

这些事件以模型任务为单位，不保存临时输出 ID 到项目数据库精确 locator 的通用映射。
项目日志因此不能直接驱动 Lua 补译；需要人工或 agent 修订时，从当前数据库按
[Lua 审查流程](../lua/README.md#4-完整审查与人工或-agent-修订)重新取得 Unit 与 locator。

WriteBack 需要人工调整布局时，每个布局单元写 Warn 事件
`write_back.manual_layout_required`。payload kind 是 `manual_layout_required`，包含受影响
逻辑单元的精确 `group_location` 与 `role`、显示区域 `region` 和采用的
`max_fullwidth_chars`；只有汇总数量不足以替代这些位置。

日志目录或文件建立失败时，JSONL 无法承载自己的失败；一旦到达可以安全写终端的位置，
stderr 立即显示包含阶段、路径或对象、操作、稳定 OS code、具体原因和处理办法的降级
诊断，不能只在内存里累计到一个可能无法送达的横幅。序列化、写入、队列关闭、writer
panic、flush 或 sync 失败时，同样由仍可用的 stderr 报告具体诊断。缓冲压力下普通事件
可以丢弃并计数；性能、失败和终态记录始终优先，不会被普通事件挤掉。最终降级诊断必须
同时给出实际丢失数量和日志路径，不能只显示“日志已降级”。

项目日志故障本身不改变业务结果、数据库状态或业务退出语义。向使用者呈现这份警告又
发生 stdout/stderr 写入、flush、后台线程或 channel 故障时，这是独立的进程呈现失败，
退出码为 `1`。

## 2. 模型任务记录

`[translation].record_translation_tasks` 省略时默认是 `true`；只有显式设为 `false` 才
关闭 Markdown 任务记录。开启时，Translate 为每个实际发出的 TaskBlock 建立：

```text
<project>/task-records/<run-id>/task-000001.md
```

它保存单个 MV/MZ 或 Generic TaskBlock 的可读消息、响应和逐 ID 验收。没有实际模型
任务时不建立空的 `<project>/task-records/<run-id>/`。格式和故障处理见
[模型任务记录规格](../translation/task-records.md)。独立 Lua 没有模型任务，因而不生成
该记录。

任务记录的渲染、建立目录、写入、flush、sync、临时文件清理或 worker 收尾失败时，若
当前项目 JSONL 仍可写，使用 Warn 事件 `observability.task_record_failed` 保存同一份结构化
安全诊断；主错误和相关清理错误分别保留。该记录处理完成后，stderr 立即显示任务记录
降级警告及其具体原因。该故障不改写模型请求、译文提交或业务结果；如果 stderr 无法呈现
警告，则按进程呈现失败返回 `1`。

JSONL 日志和任务记录都是事后记录，不是权威业务状态：它们缺失，说明不了模型请求或
数据库提交有没有发生。Markdown 也不是 Partial、Unavailable 或任务失败原因的唯一来源。

## 3. 失败与敏感信息

CLI 与 JSONL 使用同一份结构化安全诊断，保留：

- 错误 code、阶段、路径或对象；
- 具体原因与 OS、SQLite 或 HTTP 稳定代码；
- 非 2xx 标准信封中经过闭集替换和单行清理的供应商 `error.message`；
- 状态是否改变、是否已经生效、终态是否未知；
- 操作建议与恢复位置；
- 主错误、相关错误与清理错误。

日志直接沿用
[Chat Completions 规格的敏感信息闭集](chat-completions.md#6-敏感信息闭集唯一权威)，
全项目只有这一份清单。普通日志只写结构化诊断，模型正文、Lua 执行中的任意值、
SQL、参数、查询结果、游戏文本、非 2xx 原始 body 和 panic payload 都不会被自动复制；
标准供应商错误消息只作为 `DiagnosticReason::Http.provider_message` 保存；`lua.print`
只记录脚本显式提交并经过安全处理的正文。

恢复语义由数据库和目录发布 journal 各自掌管；日志只管记录，不参与补写、回滚
或重放业务操作。
