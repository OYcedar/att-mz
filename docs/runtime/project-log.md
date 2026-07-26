# ATT 项目日志现行规格

## 1. 固定位置与生命周期

项目工作区合法建立后，每次运行固定创建独立文件：

```text
<project-workspace>/logs/<run-id>.jsonl
```

日志没有配置分区，不共享活动文件、不等待跨进程日志锁、不轮转，也不按文件大小提前
停止。更早发生的 CLI 或配置错误只写 stderr。

每个运行使用单 writer。队列、批次、刷新和同步是程序内部策略；普通事件使用非阻塞
提交，缓冲饱和时丢弃本条并累计本次运行的 JSONL 丢弃计数，不能阻塞业务执行。
`performance.counters`、`failure.reported` 和 `run.finished` 使用独立可靠终态位置，
不能被普通事件压力挤掉。worker
排空普通事件后，固定按“性能计数 → primary/related 失败 → `run.finished`”写入。
正常结束前完成 flush/sync。

日志文件建立后，命令执行范围内的 panic 也必须在仍持有本次 RunId、命令、项目工作区
和日志路径的边界收敛。ATT 丢弃而不读取 panic payload，以
`internal.operation`、实际命令 stage、`impact = outcome_unknown` 和 `report_bug`
形成 CLI/JSONL 共用的安全诊断；项目日志按“性能计数 → `failure.reported` →
`run.finished`”完成收尾，`run.finished.outcome = outcome_unknown`，进程退出码为 `1`。
项目日志 runtime 即使因实现缺陷被直接丢弃，也不得留下没有终态的半截文件；其 Drop
路径必须使用预先登记的安全投影写出未知终态。日志尚未建立或根本无法建立时，进程级
panic 兜底只写 stderr。

RunId 建立失败时，本次运行不创建 JSONL，也不创建依赖 RunId 的任务记录；命令继续执行，
终态只在 stderr 显示一次非致命项目日志降级横幅及安全根因，不改变业务结果、项目状态
或退出码。日志建立、普通事件丢弃、写入或关闭失败同样不改变业务结果、项目状态或退出码，
也不递归记录自身；存在具体失败事实时，stderr 必须明确显示日志路径、失败操作和安全的
底层原因。队列丢弃没有可伪造的文件系统根因，只显示项目日志降级横幅。

## 2. Standard 翻译任务记录

Translate 可以通过 `[rpg_maker].record_translation_tasks` 开启高级可读任务记录。它按
Standard 计划中的 TaskBlock 生成文件，把一个任务的全部重试、最终 System/User、
Thinking、Assistant、逐 ID 验收和数据库提交终态放在同一份 Markdown 中：

```text
<project-workspace>/task-records/<run-id>/task-000001.md
```

该能力默认关闭；零 Standard 任务或没有启动任务时不建立空目录。Translate Lua 和独立
`lua` 命令都不生成任务记录；后者没有 LLM TaskBlock，人工候选的权威验收与提交结果由
每次 `ctx.standard.accept` 返回值和项目数据库承担。完整模板、稳定编号、互斥终态、取消、
API-key 精确替换与原子落盘规则由
[Standard 翻译任务记录现行规格](../rpg-maker/task-records.md)唯一规定。

任务记录与本节项目 JSONL 都是非权威可观测性旁路，但承担不同阅读任务：JSONL 保持
运行级结构化摘要，任务记录供人或 Agent 阅读单个 Standard 任务的完整上下文。两类故障
分别计数，并在终态各自至多显示一次降级横幅；同一次任务记录保存的主错误和清理错误
全部保留在任务记录类别内，不与 JSONL 故障互相覆盖。记录失败只在 stderr 明示路径、
操作及清理后的底层原因，不改变翻译结果、数据库、退出码、重试或后续任务。文件缺失
不能证明请求没有发生。

## 3. 闭集事件

业务代码只能提交闭集事件 code 与对应的类型化 payload，不接受任意 `message: String`。
renderer 根据 code/payload 产生本地化安全文本。事件至少携带：

| 字段 | 含义 |
|---|---|
| `time` | 事件时间 |
| `run_id` | 本次运行 ID |
| `sequence` | 运行内稳定序号 |
| `code` | 不随 locale 改变的闭集事件 code |
| `engine`、`project`、`command` | 运行上下文 |
| `payload` | 与 code 对应的结构化事实 |

`performance.counters` 的 payload 固定为 `kind = "performance"` 和一个 `snapshot`。
`snapshot` 直接序列化 `RunPerformanceSnapshot`，只包含两类 ATT 在实际执行边界可以
精确观测的闭集计数：

- SQLite 在 read snapshot、write plan、database initialization 和 interactive 四个职责范围内，
  `BEGIN` / `COMMIT` / `ROLLBACK` 控制语句的 attempted 与 succeeded 次数；
- WriteBack 完整 candidate 树校验的 started 与 completed 次数。

renderer 的本地化摘要必须明确展示 SQLite 事务控制尝试总数、candidate 完整校验
开始数和完成数。这些数值只用于观测和性能验收，不参与业务判断，不构成容量上限，
也不使日志成为恢复格式。payload 和嵌套 snapshot 都严格拒绝未知字段。

完成顺序不能改变需要稳定呈现的自然 ordinal 或主错误选择。高频文档、规则和任务事实可
逐条进入内部日志通道，但不得迫使业务模块先收集完整文本消息。

## 4. 失败记录

CLI 与 JSONL 消费同一份 `SafeDiagnostic`。每个 primary 和 related failure 各写一条
`failure.reported`，内容包括：

- 错误 code；
- stage；
- subject（路径、字段、对象或稳定身份）；
- 具体 reason 与 OS/SQLite/HTTP 稳定代码；
- impact（状态是否改变、结果是否已生效、终态是否未知）；
- action；
- recovery 位置与事实。

主错误、清理错误、shutdown 错误和恢复错误都保留，不能互相覆盖。日志不得遍历任意
`source` 链或解析 `Display` 补猜事实；安全投影必须在具体错误仍持有类型、阶段、路径和
底层代码时建立。

## 5. 内容与凭据边界

项目日志严格采用
[Chat Completions 运行根规格规定的敏感信息闭集与替换契约](chat-completions.md#6-敏感信息闭集唯一权威)，
不得在日志域另行定义或扩大清单。普通 JSONL、CLI 与 Debug 只消费职责所需的稳定结构化
摘要，不复制完整模型任务正文、Lua VM 任意正文、SQL/参数或 panic payload；这些摘要
边界用于维持稳定 schema、控制体积和控制字符，不构成新的敏感性分类。

必须保留并清理控制字符后输出安全的路径、字段、阶段、计数、配置值、HTTP 状态、
`Retry-After`、供应商稳定 code/type、SQLite primary/extended code、OS 错误码、事务与
发布终态和恢复位置。不得以敏感性为由把这些事实压成“输入错误”“项目不可用”或
“运行失败”。

项目数据库和目录发布 journal 分别拥有业务状态与恢复语义；JSONL 只用于观察和排障，
不能用于补写、回滚或重放业务操作。
