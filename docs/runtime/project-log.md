# ATT 普通项目日志现行规格

## 1. 职责与失败边界

项目日志用于运行后排障、容量观察和人工理解，不是业务事实、恢复依据或副作用门禁。
网络请求、合法候选接纳、数据库提交、目录发布、取消和成功退出都不能等待日志，也不能
因日志故障改变决定。

业务代码只提交类型化 `ProjectLogEvent`：

```rust
trait ProjectLog {
    fn emit(&self, event: ProjectLogEvent);
}
```

`emit` 是同步、不可失败且有界的调用，只尝试把事件放入队列并立即返回。日志启动失败时
使用 no-op sink；队列满、锁超时、活动文件尾部异常、序列化、写入、轮转、保留或关闭
超时只更新日志健康状态。一次进程运行最多向 stderr 输出一条本地化健康警告，且该警告
不改变退出码。

项目数据库和目录发布 journal 仍分别拥有状态收敛与恢复语义。日志记录不能用来补写、
回滚、推断或重放任何业务操作。

## 2. 配置

所有字段必填；无效、缺失或未知字段属于配置输入错误。配置一旦成功建立，后续日志运行
期故障全部按上一节降级处理。

```toml
[observability]
root = "logs"

[observability.log]
level = "info"
queue_capacity = 1024
batch_max_records = 64
batch_max_bytes = 1048576
flush_interval_ms = 100
shutdown_timeout_ms = 2000
lock_timeout_ms = 1000
max_record_bytes = 262144
max_file_bytes = 67108864
retained_rotated_files = 4
```

`level` 只接受 `error | warn | info | debug`。其余容量、字节、间隔和超时必须由配置边界
验证为当前实现可以承载的正值及组合；业务模块只接收已经验证的日志能力，不再推断预算。

## 3. 文件布局与轮转

`observability.root` 相对路径以配置文件所在目录为基准。当前布局固定为：

```text
<observability.root>/
├─ att.log.jsonl
├─ .att.log.lock
└─ att.log.00000000000000000001.jsonl
```

活动文件和轮转文件都是 UTF-8 紧凑 JSON Lines。writer 在内存中按
`batch_max_records`、`batch_max_bytes` 或 `flush_interval_ms` 形成批次，然后取得独立的
跨进程日志锁并完整追加；不为每条记录单独调用 `sync_data`。活动文件在追加当前批次将
超过 `max_file_bytes` 时安全轮转，轮转序号为 20 位十进制数。完成写入后保留最新的
`retained_rotated_files` 个轮转文件。

不完整尾行可以在同一锁内截断到最后一个完整换行；无法安全判断或修复时停止当前 writer
并记录健康故障，而不是阻塞业务。任何锁、写入、轮转或 retention 失败都不得扫描、改写
或删除无关文件。既有 `audit*.jsonl` 文件不识别、不转换且不自动删除。

## 4. 记录契约

每行包含以下稳定字段：

| 字段 | 含义 |
|---|---|
| `time` | 记录生成时间 |
| `level` | `error | warn | info | debug` |
| `code` | 不随 locale 改变的稳定事件 code |
| `process_id` | 当前进程 ID |
| `run_id` | 可空运行 ID；生成失败不得阻止命令 |
| `sequence` | 同一运行内单调递增序号 |
| `engine` | 可空 `mv | mz` |
| `project` | 可空项目名 |
| `command` | 可空命令名 |
| `profile` | 可空实际 Translate Profile |
| `locale` | 本条 `message` 使用的 UI locale |
| `message` | 面向人的本地化消息 |
| `payload` | 与 code 对应的类型化、语言无关事实 |

同一事件在不同 UI locale 下只允许 `locale` 和 `message` 改变；`code` 与 `payload` 必须
保持一致。业务层产生方案来源、阶段、计数、部分结果和发布终态等结构化事实，日志边界
负责选择级别、格式化消息和序列化，不由业务模块拼接 JSON 或本地化句子。

Translate 的 `run_plan` payload 分别保存 Profile 的 `source` 与 Lua 的 `lua_source`；显式
Profile 和项目状态 Lua 可以在同一方案中并存，不能压成一个整体来源。最终方案事务的
稳定 code 分别为 `run_plan.save_failed`（确认未保存）、
`run_plan.save_outcome_unknown`（提交终态未知）和
`run_plan.saved_finalization_failed`（已提交但收尾失败），不得用同一失败事实代替。

日志严格排除 API key、Authorization Header、完整 Client parameters、Prompt、完整
messages、模型正文、原文和译文。路径、Profile、项目名和其他允许记录的用户文本先移除
ESC、换行伪装及双向文本控制字符，并由本地化层进行方向隔离。

## 5. 事件密度

默认 `info` 只记录足以理解一次运行的低密度事实：

- 运行与命令阶段；
- 显式输入、项目状态或产品行为产生的逐字段方案来源；
- 重试摘要，不记录请求或响应正文；
- 无工作及“无需调用模型”的准确原因；
- Complete、Partial、Unavailable 和人工处理摘要；
- 目录发布与运行方案保存终态。

单个翻译任务、单份文档、单条规则和其他高频工作细节只进入 `debug`。日志级别不会改变
业务执行、最终摘要、进度计数或错误分类。

## 6. 生命周期

日志可以在 CLI 与配置成功建立后开始接收事件。worker 在独立线程中消费有界队列；
`shutdown_timeout_ms` 只限制退出时等待日志批次的时间。超时后命令按原业务终态退出，
不等待无限排空，也不把未写记录解释为业务失败。

最终 stdout 摘要、stderr 错误和 Ctrl-C 的 `130` 均由业务结果决定。即使整个运行使用
no-op sink，也必须能够完成与启用日志时相同的网络请求、数据库状态、目录发布和运行
方案替换。
