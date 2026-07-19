# OpenAI-compatible Chat Completions 运行根现行规格

## 1. 职责

`OpenAiChatCompletionExecutor` 按受信 Client 执行一次非流式 Chat Completions 请求。它拥有进程内全局并发、有限队列、连接池、超时、客户端 RPM/burst 和实际 HTTP 协议；不自动重试、不探测供应商、不修复响应。

Standard 的有限重试属于 RPG Maker 翻译策略；Translate Lua 自己决定调用和重试。二者共享同一个 Executor、Client、连接池、全局容量和 RPM/burst。

## 2. 请求

默认正文严格只有：

```json
{
  "model": "client-model",
  "messages": [],
  "stream": false
}
```

Client 的严格 JSON `parameters` 顶层字段随后合并；程序不解释 `n`、token 上限或供应商私有字段。只有 `model`、`messages`、`stream` 不得被覆盖。API key 固定作为 Bearer Header 发送，不进入 Debug、错误、审计或输出。

准入顺序为完整序列化、总容量（活动+队列）、Client RPM/burst、活动许可、单次 HTTP。队列已满或准入超时返回 Retryable；Future 取消通过 RAII 归还尚未进入 HTTP 的许可。Client `timeout_ms` 限制完整请求，连接和连续读取超时限制相应网络阶段。

## 3. 供应商成功信封

HTTP 200 后只严格要求实际消费的核心：

- 正文是一个完整 JSON 值；
- `choices` 中恰有一个 choice 的 `index` 是数值 `0`；
- 该 choice 的 `message.content` 与 `finish_reason` 是字符串。

不检查成功响应 `Content-Type`，不要求 `message.role`，并忽略其他 choice 和供应商扩展字段中不被消费的内容。多个 choice 可以存在，但数值 `index == 0` 必须恰好一个。

元数据采用宽松读取：

- `x-request-id` 缺失或不能作为有效字符串使用时为 `None`；
- 正文 `id` 缺失、null 或类型错误时为 `None`；
- `usage` 缺失、null、不完整或类型错误时整体为 `None`。

因此 `provider_request_id` 与 `provider_response_id` 都是可选值，互不补位；审计写 `null`，Lua 返回 `nil`。`final_response_usage` 只表示最终成功 HTTP 响应可用的 usage，不声称覆盖失败尝试或完整计费。

这里的宽松只针对第三方供应商 HTTP 信封。`message.content` 内由模型生成的 RPG Maker 翻译正文仍执行完整顶层数组、强类型元素、ID、ATT token、语言和逐 ID 内容验收，二者不得混为一层。

## 4. 失败分类

Retryable 包含队列满、准入超时、DNS/连接/发送/读取超时或连接中断，以及 HTTP 408、429、500、502、503、504。`Retry-After` 支持非负秒数和 HTTP-date，根只返回该事实，不自行等待或重试。

Fatal 包含请求序列化/构造、TLS/证书、其他 HTTP 状态，以及 HTTP 200 正文不是完整 JSON、没有唯一数值 index 0、所选 content/finish_reason 不是字符串。非 200 响应不保存完整错误正文。

## 5. 生命周期与隐私

shutdown 停止新准入并唤醒尚未进入 HTTP 的等待者；已开始的请求继续到单次明确终态。所有路径归还容量，shutdown 等待活动请求结束。

错误、Debug 和审计不包含 API key、完整 messages、完整请求正文、完整响应、原文或译文。Client Debug 不显示秘密或完整 parameters 值。
