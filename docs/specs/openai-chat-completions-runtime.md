# OpenAI-compatible Chat Completions 运行根现行规格

## 1. 职责与范围

`OpenAiChatCompletionExecutor` 是公共 `LlmRequestExecutor` 的生产实现。它按受信
`OpenAiChatCompletionClient` 只执行一次非流式 OpenAI-compatible Chat Completions
HTTP 请求，负责请求大小、全局容量、客户端速率、连接池、单次超时、响应大小和
HTTP 协议的精确边界。

该根不自动重试、不自动探测供应商、不修复响应，也不支持流式、多 choice、tool call、图像或音频。Standard 翻译对 Retryable 失败的有限重试属于上层业务策略；Translate Lua 完整拥有自身的调用与重试逻辑。

## 2. 构造与信任边界

进程为当前 Translate 命令构造一个共享 Executor。全局运行配置精确建立：

- 活动请求上限、排队容量和准入超时；
- 连接超时、连续读取超时、空闲连接池时间和每主机空闲连接上限；
- 关闭系统代理后的显式代理选择；
- Windows native TLS 根之上显式增加的 PEM 根证书。

每个受信 Client 精确建立 endpoint、直接 Bearer 或无认证、model、单请求超时、
请求/成功响应/错误响应三类字节上限、RPM、burst 和用户提供的额外 JSON 请求字段。
这些静态不变量已由统一配置边界建立，Executor 不重复解释或校验。Standard 与
Translate Lua 共享同一个 Executor 和同一个 Client 对象，因此共享 HTTP 连接池、
总容量和客户端速率额度。

endpoint 只允许：

- 任意 HTTPS 主机；
- 配置明确允许时的 `localhost`、IPv4 loopback 或 IPv6 loopback HTTP。

endpoint 不得嵌入用户名、密码或 fragment。根不跟随重定向，不读取系统代理。代理只能关闭或由配置显式指定。

## 3. 请求 wire

请求正文是紧凑 UTF-8 JSON。固定字段为：

```json
{
  "model": "client-model",
  "messages": [
    { "role": "system", "content": "..." },
    { "role": "user", "content": "..." }
  ],
  "stream": false
}
```

`model`、`messages` 与 `stream=false` 是根拥有的完整固定字段集。Client 的
`request_body_extra` 为 `{}` 时，请求中不得出现 `n`、`max_tokens`、
`max_completion_tokens` 或其他隐式字段。非空时，根只把已验证 JSON 对象的顶层
字段合并进正文；`n`、两种 token 上限以及供应商私有字段都不被解释或改写。
用户配置 `n=2` 时会按原值发送，但成功响应仍必须满足本规格的单 choice wire。

根通过受限 writer 序列化正文；一旦下一段输出会超过 `max_request_bytes` 就立即停止，不先建立超限的完整缓冲区。序列化成功后才占用任何请求许可。认证只使用受信 Client 内已经转换为秘密值的可选直接 Bearer。

## 4. 准入、取消与资源顺序

每次请求按以下顺序进入网络：

```text
有界序列化与请求字节上限
        ↓
总容量许可（active + queue）
        ↓
Client RPM / burst
        ↓
active 许可
        ↓
单次 HTTP 请求与有界响应读取
```

总容量已满时立即返回 Retryable `QueueFull`。速率等待与 active 等待共用一个绝对准入截止时间，任一阶段超时都返回 Retryable。速率额度在 active 容量之前消耗；已获取的总容量和 active 许可通过 RAII 在成功、失败或 Future 被取消时归还。

## 5. 成功响应

成功必须同时满足：

- HTTP 状态为 200；
- `Content-Type` 是 `application/json` 或 `application/*+json`；
- 正文在 `max_response_bytes` 内；
- 正文 `id` 是字符串；
- `choices` 恰好一项，`index == 0`，`message.role == "assistant"`；
- `message.content` 和 `finish_reason` 是字符串；
- `usage` 可缺席或为 null；存在时必须完整包含三个非负整数。

供应商扩展字段被忽略，必需字段的类型漂移不被接受。

`LlmResponse` 保留两个不同的身份：

- `provider_request_id`：可选的 HTTP `x-request-id` 响应头；
- `provider_response_id`：必需的正文 `id`。

两者不相互补位。`final_response_usage` 只代表当前任务最终成功 HTTP 响应报告的 usage，不声称覆盖失败尝试、Lua 调用或完整计费量。这些元数据作为一个整体进入任务结果和翻译日志。

## 6. 失败分类

Retryable 包含：

- 总队列已满；
- 速率或 active 准入超时；
- DNS、TCP 连接、发送、连续读取超时或连接中断；
- HTTP 408、429、500、502、503、504。

Fatal 包含：

- 请求序列化、构造或字节上限失败；
- TLS 或证书失败；
- 其他 HTTP 状态；
- 成功响应过大、Content-Type 无效、JSON 无效或 wire 不符合契约；
- Executor 已进入 shutdown。

`Retry-After` 同时支持非负秒数和 HTTP-date。它只作为 Retryable 事实返回，根不因此自动等待或重试。

错误状态正文独立受 `max_error_response_bytes` 限制。根只保留已读字节数、是否截断和是否读取失败，不保留或展示错误正文。

## 7. 隐私与可观测性

根的 `Display`、`Debug` 和错误链不包含 Bearer 密钥、messages、完整请求正文、
完整成功响应或错误响应。Client Debug 只显示额外 JSON 的顶层字段名，不显示
字段值；Bearer 和显式代理一律脱敏。

持久翻译任务日志可记录 `provider_request_id`、`provider_response_id` 和 `final_response_usage`，不记录密钥、messages、模型正文、原文或译文全文。

## 8. shutdown

`shutdown()` 先原子停止准入，再唤醒正在等待速率或 active 许可的请求。这些尚未进入 HTTP 的请求返回 shutdown 失败并归还容量。已进入 HTTP 的请求继续到单次明确终态；shutdown 等待所有已准入作业归还许可后结束。

生命周期通知使用可保留最新状态的 watch 通道，因此停止和空闲事实不依赖一次性唤醒时机，不会因为订阅与通知交错而无限等待。
