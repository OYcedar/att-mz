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
- `lua.summary`：数据库调用次数、changed rows、译文操作次数和打印行数。

Rules command 直接参数出现可跳过的非字符串时，Extract 成功后写 Warn 事件
`extract.rules.command_non_string_skipped`。payload kind 是
`rules_command_non_string_skipped`，只保存 `rule_number`、`source_file`、
`command_code`、`parameter`、`actual_type` 和 `skipped_count`；不保存原始参数值或
逐命令位置。相同键已经聚合，事件按键稳定排序。

SQL、参数、查询结果、Lua 变量和游戏正文不会自动变成日志事件；只有脚本显式
`print` 的内容——有意输出——会作为 `lua.print` 保存。脚本只打印需要保留的
内容就好。

正常结束前，writer 先排空普通事件，再依次写性能计数、主错误与相关错误、唯一
`run.finished`，并完成 flush/sync。panic 在仍持有运行上下文的边界转换成安全
诊断，`run.finished.outcome` 为 `outcome_unknown`。

日志建立、写入或关闭失败时，stderr 显示一次降级诊断，业务结果、数据库和退出码
都不受牵连。缓冲压力下普通事件可以丢弃并计数；性能、失败和终态记录始终优先，
不会被普通事件挤掉。

## 2. 模型任务记录

`[translation].record_translation_tasks = true` 时，Translate 还可以建立：

```text
<project>/task-records/<run-id>/task-000001.md
```

它保存单个 MV/MZ 或 Generic TaskBlock 的可读消息、响应和逐 ID 验收。格式和故障
处理见[模型任务记录规格](../translation/task-records.md)。独立 Lua 没有模型
任务，因而不生成该记录。

JSONL 日志和任务记录都是事后记录，不是权威业务状态：它们缺失，说明不了模型
请求或数据库提交有没有发生。

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
