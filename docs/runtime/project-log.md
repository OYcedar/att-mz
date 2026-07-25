# ATT 项目日志现行规格

## 1. 固定位置与生命周期

项目工作区合法建立后，每次运行固定创建独立文件：

```text
<project-workspace>/logs/<run-id>.jsonl
```

日志没有配置分区，不共享活动文件、不等待跨进程日志锁、不轮转，也不按文件大小提前
停止。更早发生的 CLI 或配置错误只写 stderr。

每个运行使用单 writer。队列、批次、刷新和同步是程序内部策略；普通事件在缓冲饱和时
背压而不是丢弃。`performance.counters`、`failure.reported` 和 `run.finished` 使用独立
可靠终态位置，不能被普通事件压力挤掉。worker
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

日志建立、写入或关闭失败不改变业务结果、项目状态或退出码，也不递归记录自身；stderr
必须明确显示日志路径、失败操作和安全的底层原因。

## 2. 敏感 LLM 调用记录

Translate 可以通过 `[llm]` 中可选的 `record_calls` 开启供人工排障的敏感调用记录。开关省略
或为 `false` 时不创建调用记录；为 `true` 时，第一次实际 HTTP 调用按本次运行与普通
项目日志共享的 RunId 建立：

```text
<project-workspace>/llm-calls/<run-id>/call-000001.md
```

Standard 的每次请求尝试（包括重试）和 Translate Lua 发起的每次请求都各占一个从 1
开始编号的 Markdown 文件。零次模型调用不建立空的 `llm-calls/<run-id>` 目录。

每个文件以人类可读形式记录实际发送的最终请求和 Provider 结果：敏感内容警告、调用
时间与耗时、移除用户信息、查询参数和 fragment 的 endpoint、合并 Client parameters
后的完整请求 JSON，以及传输结果、HTTP 状态和已读取的 UTF-8 响应正文。无法按 UTF-8
显示的响应会明确记录字节数并使用替换字符展示。请求内容必须来自发送边界使用的最终
结构，不能从模板或业务输入反推。记录器不会读取或另行写入 API key、Authorization、
完整请求 Header、代理凭据和 TLS 凭据；最终请求 JSON 或 Provider 正文自身若含有同样
的文本，仍会作为调用内容保留。

这些文件包含 Prompt、原文、译文、模型输出和 Client 自定义参数，必须视为本机敏感
材料，不应提交版本库或直接对外分享。它们是非权威诊断旁路，不参与请求、重试、响应
解析、ATT 验收、数据库提交、恢复或重放；建立或写入失败只在 stderr 明示路径、操作和
安全的底层原因，不改变业务结果、项目状态或退出码。缺少某个调用文件不能证明请求没有
发生，也不能区分关闭记录、尚未调用和记录失败。

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

## 5. 安全边界

普通 JSONL、CLI、Debug 和安全诊断始终隐藏 API key、授权 Header 值、Client 参数值、
Prompt/messages、模型正文、原文与译文、Lua 正文/VM 任意文本、SQL/参数和 panic
payload。第 2 节的敏感 LLM 调用记录是 Prompt、模型正文和 Client 参数值的唯一例外，
记录器仍不从请求元数据读取凭据或授权 Header。

必须保留并清理控制字符后输出安全的路径、字段、阶段、计数、配置值、HTTP 状态、
`Retry-After`、供应商稳定 code/type、SQLite primary/extended code、OS 错误码、事务与
发布终态和恢复位置。不得以防泄密为由把这些事实压成“输入错误”“项目不可用”或
“运行失败”。

项目数据库和目录发布 journal 分别拥有业务状态与恢复语义；JSONL 只用于观察和排障，
不能用于补写、回滚或重放业务操作。
