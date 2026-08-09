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
约束。Translate 按 Client `retry_delays_ms` 执行有限重试；本地等待不消耗重试次数。普通
429 的 `Retry-After` 对同一 Client 的所有请求生效：等待结束前，其他 worker 也不能发出
下一次请求。

运行根把 HTTP 状态、供应商稳定 code/type、标准 `error.message`、`Retry-After` 和类型化
失败事实交给调用方，让多次逻辑 attempt 能汇总到同一条任务记录。服务状态只根据 HTTP
状态和允许识别的供应商 code/type 分类，绝不解析 `error.message` 或其他错误正文猜测认证、
额度或账户状态。三个供应商字段彼此
独立：其中一个缺失或类型错误，不会抹掉另外两个合法字符串。只读取顶层
`error` 对象，不猜测顶层 `message`、`detail`、`error_description` 或纯文本正文。
原始 Header 和任意非 200 wire body 留在运行根内部。

请求层和 Translate 调度层共享同一次运行的停止事实：

- HTTP 401、403，或允许识别的永久额度、账户错误一经确认，就停止后续请求和 Task
  准入；本次 Translate 为 Failed，退出码为 `1`；
- 普通 429 等待超过 `max_retry_after_ms` 或重试耗尽时，当前 Task 为 Unavailable，并停止
  后续请求和 Task 准入；Translate 为 Incomplete，退出码仍为 `0`；
- DNS、连接、读取、超时、HTTP 500 等普通可重试问题耗尽时，只有当前 Task 为
  Unavailable，后续 Task 仍可继续；
- 停止事实确认前已经收到有效响应的在途 Task 仍按自然顺序验收和提交；已经准入但尚未
  发出下一次 HTTP 的 worker 停止，不把它伪装成一次新模型调用。

任务准入满足 `planned = started + not_started`。服务停止后没有开始的任务计入
`not_started`；不得把请求门拒绝误计为一次已开始任务，也不得补造完成进度。

Fatal 包含请求构造失败、TLS/证书问题、其他 HTTP 状态，以及不满足成功信封的 200
响应。安全诊断会说明 HTTP 状态、`Retry-After`、供应商 code/type，以及标准信封中
经过闭集替换和单行清理的 `error.message`。完整错误正文不落盘。

内部错误区分 DNS、连接、发送、读取、TLS、timeout、HTTP status、响应 JSON 和成功信封
错误。进入 CLI、项目日志和任务记录前，只呈现 Endpoint 对象、直接原因、状态影响和处理办法；必要
时附 HTTP 状态、Retry-After、供应商 code/type 或 JSON 行列。Endpoint 只公开 scheme、host
和可选 port，不记录 path、query、凭据或供应商请求 ID，也不从后端正文解析内部状态。

## 5. 生命周期

shutdown 关上入口：新请求不再开始，尚未进入 HTTP 的等待者被唤醒；已经在 HTTP
中的请求继续走到明确终态。无论走哪条路径，活动许可都通过 RAII 如数归还。

收到非 200 响应头后，运行根先暂停该 Client 的替补请求，再读取和分类正文。401、403
可直接确认永久停止；其他非 200 在类型化分类完成前也不会释放替补准入。普通 500 分类
完成后恢复准入；429 进入共享等待或停止；永久额度、账户错误关闭入口。这个决定门保证
慢错误正文不会让实际调用数突破错误发生时已经活动的请求窗口。

## 6. 敏感信息闭集唯一权威

本节是 ATT 现行敏感信息闭集、替换语义与内容边界的唯一权威。闭集只有一个成员：
本次实际选中 LLM Client 的 API key 实际值。Prompt、原文、译文、自定义参数、
Thinking、Assistant、Provider 正文和普通用户内容都按普通内容处理。这份清单在
任何模块、日志、诊断、文档、测试或 Skill 中都保持原样，不增不减，也不另行复述。

运行根的 Debug、CLI 和普通 JSONL 用稳定摘要说话：Client ID、阶段、Endpoint
对象、HTTP 状态、超时种类，以及经过处理的标准供应商错误字段都可以出现；公开诊断只用
对象、原因、影响和处理办法呈现；
闭集值与完整请求、原始响应、Header 不出现。`error.message` 先精确替换当前 API key，
再删除终端控制和双向控制字符并收敛为单行；没有可见内容时省略。这是运行根职责、
稳定 schema、控制字符和输出体积的边界，与敏感性分类无关。

翻译任务记录只保存实际 User message、原始 Assistant 和最终任务结果，并在这些正文与
诊断中应用同一个闭集替换器。每个精确匹配片段都替换为：

```text
[REDACTED API KEY]
```

替换器只碰命中的闭集值：所在字段、段落和相邻正文保持原样，替换标记本身也不再处理。
配置、Endpoint、Model、parameters、Header、Provider 外层信封和非 200 原始 body 不进入
任务记录。`Authorization` 是一个字段和一种认证方案，不是另一类敏感信息；诊断需要说明
认证事实时，保留字段名和方案就够了。任务记录的其余格式契约见
[模型任务记录现行规格](../translation/task-records.md)。
