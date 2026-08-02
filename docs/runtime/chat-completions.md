# OpenAI-compatible Chat Completions 运行根现行规格

## 1. 职责

`OpenAiChatCompletionExecutor` 按所选 Client 执行非流式请求，拥有共享 HTTP 连接池、
Client 活动请求许可、可选 RPM limiter、连接/读取/完整请求超时和实际 HTTP 协议。
MV、MZ 与 Generic Translate 共享同一 Client 约束。

运行根专注做好一件事：执行请求，并把结构化传输事实交还调用方。TaskBlock、引擎
value 形状、逐 ID 验收、数据库提交状态和任务记录各有负责的翻译域；独立 Lua 走
自己的通道，不经过运行根。

请求只受两样东西控制：Client 的 `max_concurrent_requests` 和可选 `rate_limit`。
ATT 不叠加第二层用户队列、总容量或本地准入截止时间；等待活动许可或 RPM 时只响应
取消/shutdown，这种等待既不是模型失败，也不消耗重试。响应完成后立即归还 Client
的 HTTP 许可；Translate 自己的确定性顺序提交窗口是独立的内部背压边界，不计作
活动 HTTP 请求，也不出现在用户配置里。

## 2. 请求

基础正文是：

```json
{
  "model": "client-model",
  "messages": [],
  "stream": false
}
```

Client 的严格 JSON `parameters` 随后并入正文；`model`、`messages`、`stream` 由
ATT 保留，parameters 中不得出现这三个键。供应商私有字段原样发给供应商，ATT 自己
不解释。API key 只以 Bearer Header 的形式发送。

`connect_timeout_ms` 管建立连接，`read_timeout_ms` 管连续读取，
`request_timeout_ms` 管完整 HTTP 请求；本地许可和限速等待属于调度，不在它们的
计时范围内。

## 3. 成功信封

HTTP 200 后要求：

- 正文是一个完整 JSON 值；
- `choices` 中恰有一个数值 `index == 0`；
- 该 choice 的 `message.content` 与 `finish_reason` 是字符串。

其余 choice 和未消费的供应商扩展字段原样忽略。`x-request-id`、正文 `id` 与
`usage` 按宽松可选方式读取：缺失或类型不符时为 `None`，三者互不补位。拿到
`message.content` 后，对应引擎继续执行完整 ID、value 形状、ATT token、语言和
逐 ID 验收。

## 4. 失败与重试

Retryable 包含 DNS/连接/发送/读取/完整请求超时或中断，以及 HTTP 408、429、500、
502、503、504。`Retry-After` 支持秒数和 HTTP-date，并受 Client `max_retry_after_ms`
约束。Translate 按 Client `retry_delays_ms` 执行有限重试；本地等待不消耗重试次数。
运行根把 HTTP 状态、供应商稳定 code/type、标准 `error.message`、`Retry-After` 和
类型化 `HttpIssue` 交给调用方，让多次逻辑 attempt 能汇总到同一条任务记录。三个供应商字段
彼此独立：其中一个缺失或类型错误，不会抹掉另外两个合法字符串。只读取顶层
`error` 对象，不猜测顶层 `message`、`detail`、`error_description` 或纯文本正文。
原始 Header 和任意非 200 wire body 留在运行根内部。

Fatal 包含请求构造失败、TLS/证书问题、其他 HTTP 状态，以及不满足成功信封的 200
响应。安全诊断会说明 HTTP 状态、`Retry-After`、供应商 code/type，以及标准信封中
经过闭集替换和单行清理的 `error.message`。完整错误正文不落盘。

`HttpIssue` 使用稳定 code 区分 DNS、连接、发送、读取、TLS、timeout、HTTP status、响应
JSON 和成功信封错误。transport 保存经过清理的 Endpoint 对象、发生阶段、transport kind、
可用的 I/O kind 与 raw OS code；status 还保存非成功响应正文读取失败的独立阶段和传输
类别。Endpoint 只公开 scheme、host 和可选 port，不记录 path、query 或凭据。JSON 失败
保存类别、行和列；成功信封失败保存封闭 violation。诊断 code、stage 和 resolution 由
具体 issue 唯一推导，不从后端错误正文解析。

## 5. 生命周期

shutdown 关上入口：新请求不再开始，尚未进入 HTTP 的等待者被唤醒；已经在 HTTP
中的请求继续走到明确终态。无论走哪条路径，活动许可都通过 RAII 如数归还。

## 6. 敏感信息闭集唯一权威

本节是 ATT 现行敏感信息闭集、替换语义与内容边界的唯一权威。闭集只有一个成员：
本次实际选中 LLM Client 的 API key 实际值。Prompt、原文、译文、自定义参数、
Thinking、Assistant、Provider 正文和普通用户内容都按普通内容处理。这份清单在
任何模块、日志、诊断、文档、测试或 Skill 中都保持原样，不增不减，也不另行复述。

运行根的 Debug、CLI 和普通 JSONL 用稳定摘要说话：Client ID、阶段、Endpoint
对象、HTTP 状态、超时种类、`DiagnosticReport.effect` 和经过处理的标准供应商错误字段都可以出现；
闭集值与完整请求、原始响应、Header 不出现。`error.message` 先精确替换当前 API key，
再删除终端控制和双向控制字符并收敛为单行；没有可见内容时省略。这是运行根职责、
稳定 schema、控制字符和输出体积的边界，与敏感性分类无关。

翻译任务记录使用实际选中的 Endpoint、Model、parameters 和最终消息，并在所有
可读字段中递归应用同一个闭集替换器。Endpoint query、自定义参数键和值、System、
User、输入历史 Assistant、Thinking、输出 Assistant、Provider 标识和任务诊断中
的每个精确匹配片段都替换为：

```text
[REDACTED API KEY]
```

替换器只碰命中的闭集值：所在字段、段落和相邻正文保持原样，替换标记本身也不再
处理。配置中的 API key 字段从不进入任务记录；任务记录也不采集 Header、Provider
外层信封或非 200 原始 body，只保留上述标准错误字段投影。`Authorization` 是一个字段
和一种认证方案，不是另一类敏感信息；诊断需要说明认证事实时，保留字段名和方案就
够了。任务记录的其余格式契约见[模型任务记录现行规格](../translation/task-records.md)。
