# OpenAI-compatible Chat Completions 运行根现行规格

## 1. 职责

`OpenAiChatCompletionExecutor` 按所选 Client 执行非流式请求，拥有共享 HTTP 连接池、
Client 活动请求许可、可选 RPM limiter、连接/读取/完整请求超时和实际 HTTP 协议。
Standard 与 Translate Lua 共享同一 Client 约束。

运行根只执行请求并返回结构化传输事实；Standard TaskBlock、Lua 私有业务、逐 ID 验收、
数据库提交状态和高级任务记录均由各自语义所有者负责。Standard 的高级任务记录由
RPG Maker 顺序最终化边界拥有；Translate Lua 不生成该记录。

只有 Client 的 `max_concurrent_requests` 和可选 `rate_limit` 控制请求。没有第二层用户队列、
总容量或本地准入截止时间；等待活动许可或 RPM 时只响应取消/shutdown，不算模型失败或
重试。响应完成后立即释放 Client 的 HTTP 许可；Translate 自己的确定性顺序提交窗口是
独立的内部背压边界，不计作活动 HTTP 请求，也不进入用户配置。

## 2. 请求

基础正文是：

```json
{
  "model": "client-model",
  "messages": [],
  "stream": false
}
```

Client 的严格 JSON `parameters` 随后合并；`model`、`messages`、`stream` 不得覆盖。程序
不解释供应商私有字段。API key 只作为 Bearer Header 发送。

`connect_timeout_ms` 限制建立连接，`read_timeout_ms` 限制连续读取，
`request_timeout_ms` 限制完整 HTTP 请求。它们不限制本地许可或限速等待。

## 3. 成功信封

HTTP 200 后要求：

- 正文是一个完整 JSON 值；
- `choices` 中恰有一个数值 `index == 0`；
- 该 choice 的 `message.content` 与 `finish_reason` 是字符串。

其他 choice 和未消费供应商扩展字段忽略。`x-request-id`、正文 `id` 与 `usage` 使用宽松
可选读取；缺失或类型不符时为 `None`，互不补位。模型 `message.content` 仍由 RPG Maker
层执行完整 ID、数组形状、ATT token、语言和逐 ID 验收。

## 4. 失败与重试

Retryable 包含 DNS/连接/发送/读取/完整请求超时或中断，以及 HTTP 408、429、500、502、
503、504。`Retry-After` 支持秒数和 HTTP-date，并受 Client `max_retry_after_ms` 约束。
Standard 按 Client `retry_delays_ms` 执行有限重试；本地等待不消耗重试次数。
运行根向调用方提供 HTTP 状态、供应商稳定 code/type、`Retry-After` 和结构化原因，
使 Standard 能把多次逻辑 attempt 汇总到同一任务记录；它不提供原始 Header 或任意
非 200 wire body。

Fatal 包含请求构造、TLS/证书、其他 HTTP 状态，以及 200 响应不满足成功信封。安全诊断
公开 HTTP 状态、`Retry-After` 和允许公开的供应商 code/type，但不保存任意错误正文。

## 5. 生命周期

shutdown 停止新请求并唤醒尚未进入 HTTP 的等待者；已进入 HTTP 的请求继续到明确终态。
所有路径通过 RAII 归还活动许可。

## 6. 敏感信息闭集唯一权威

本节是 ATT 现行敏感信息闭集、替换语义与内容边界的唯一权威。闭集只有本次实际选中
LLM Client 的 API key 实际值。Prompt、原文、译文、自定义参数、Thinking、Assistant、
Provider 正文和普通用户内容不因内容类别成为敏感信息。任何模块、日志、诊断、文档、
测试或 Skill 都不得扩大、缩小或另行复述这份闭集。

运行根的 Debug、CLI 和普通 JSONL 不得显示闭集值；它们保持 Client ID、阶段、Endpoint
对象、HTTP 状态、超时种类和状态影响等稳定摘要，不复制完整请求、响应或 Header。这是
运行根职责、稳定 schema、控制字符和输出体积边界，不构成新的敏感性分类。

Standard 任务记录使用实际选中的 Endpoint、Model、parameters 和最终消息，并在所有
可读字段中递归应用同一个闭集替换器。Endpoint query、自定义参数键和值、System、User、
输入历史 Assistant、Thinking、输出 Assistant、Provider 标识和任务诊断中的每个精确
匹配片段都替换为：

```text
[REDACTED API KEY]
```

替换只作用于命中的闭集值，不删除或改写所在字段、段落和相邻正文，也不对替换标记进行
二次处理。配置中的 API key 字段本身完全不进入任务记录；任务记录不采集 Header、
Provider 外层信封或非 200 原始 body。`Authorization` 是字段与认证方案，不是另一类
敏感信息；诊断需要说明认证事实时只保留字段名和方案。任务记录的其余格式契约见
[Standard 翻译任务记录现行规格](../rpg-maker/task-records.md)。
