# OpenAI-compatible Chat Completions 运行根现行规格

## 1. 职责

`OpenAiChatCompletionExecutor` 按所选 Client 执行非流式请求，拥有共享 HTTP 连接池、
Client 活动请求许可、可选 RPM limiter、连接/读取/完整请求超时和实际 HTTP 协议。
Standard 与 Translate Lua 共享同一 Client 约束。

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

Fatal 包含请求构造、TLS/证书、其他 HTTP 状态，以及 200 响应不满足成功信封。安全诊断
公开 HTTP 状态、`Retry-After` 和允许公开的供应商 code/type，但不保存任意错误正文。

## 5. 生命周期与隐私

shutdown 停止新请求并唤醒尚未进入 HTTP 的等待者；已进入 HTTP 的请求继续到明确终态。
所有路径通过 RAII 归还活动许可。

Debug、CLI 和 JSONL 不包含 API key、Header 值、完整 Client parameters、Prompt/messages、
完整请求/响应、模型正文、原文或译文。它们仍必须显示安全的 Client ID、阶段、URL 对象、
HTTP 状态、超时种类和状态影响。
