# OpenAI-compatible 模型 HTTP 运行根现行规格

## 1. 职责

`OpenAiCompatibleExecutor` 按所选 Client 执行非流式或流式 Chat Completions、Responses
请求，拥有共享 HTTP 连接池、Client 活动请求许可、可选 RPM limiter、连接/读取/完整请求
超时和实际 HTTP 协议。MV、MZ 与 Generic Translate 共享同一 Client 约束。

运行根专注做好一件事：执行请求，并把结构化传输事实交还调用方。TaskBlock、引擎
value 形状、逐 ID 验收、数据库提交状态和任务记录各有负责的翻译域；独立 Lua 走
自己的通道，不经过运行根。
所选 Client 及其 Endpoint、Model、协议、流式选择和参数只描述本次及后续请求，不参与数据库中
既有译文正文的 Current 判断。

外部请求准入只受两样东西控制：Client 的 `max_concurrent_requests` 和可选 `rate_limit`。
ATT 不叠加第二层用户队列、总容量或本地准入截止时间；等待活动许可或 RPM 时只响应
取消/shutdown，这种等待既不是模型失败，也不消耗重试。完整 JSON 或 SSE 流形成明确终态后
立即归还 Client 的 HTTP 许可；Translate 自己的确定性顺序提交窗口是独立的内部背压边界，不计作
活动 HTTP 请求，也不出现在用户配置里。

## 2. 请求

Client 的 `protocol` 取 `chat_completions` 或 `responses`；省略时默认
`chat_completions`，Responses 必须显式选择。协议只由该字段决定，不根据 URL、模型名、
HTTP 失败或响应正文猜测，也不在失败后尝试另一个协议。

Client 的 `stream` 是必填布尔值。`false` 使用完整 JSON 响应，`true` 使用 SSE 增量响应；
它只改变 HTTP 接收和协议解析，不改变翻译响应、候选验收、提交或任务记录语义。

`url` 接受服务基础地址或已经包含协议路径的完整端点。ATT 保留配置中的 scheme、host、port、
路径前缀和 query，移除末尾斜杠；如果路径以 `/chat/completions` 或 `/responses` 结尾，先移除
该已知后缀，再按所选协议追加 `/chat/completions` 或 `/responses`。其他路径直接作为基础路径
追加协议后缀。ATT 不自行插入 `/v1` 或其他供应商版本路径。

Chat Completions 基础正文是：

```json
{
  "model": "client-model",
  "messages": [],
  "stream": false
}
```

Responses 基础正文是：

```json
{
  "model": "client-model",
  "input": [],
  "stream": false,
  "background": false
}
```

上面展示发行模板的 `stream = false`。配置为 `true` 时，ATT 只把正文中的 `stream` 改为
`true`，其余请求字段及合并顺序不变。

调用方建立的 system 与 user message 按原顺序进入 Chat Completions 的 `messages` 或
Responses 的 `input`，role 与字符串 content 保持不变。Client 的严格 JSON `parameters`
随后并入正文；`model`、`stream` 及所选协议的 `messages` 或 `input` 由 ATT 保留，Responses
的 `background` 固定为 `false`，这些字段都不得由 parameters 覆盖。ATT 不执行后台任务轮询。
供应商私有字段原样发给供应商，ATT 自己不解释。API key 只以 Bearer Header 的形式发送。

`connect_timeout_ms` 管建立连接，`read_timeout_ms` 管连续读取完整 JSON 或 SSE 数据，
`request_timeout_ms` 管完整 HTTP 请求；本地许可和限速等待属于调度，不在它们的
计时范围内。

## 3. 成功响应

HTTP 200 后按 Client 的 `stream` 选择唯一响应格式。`false` 要求正文是一个完整 JSON
object；`true` 按 SSE 事件读取。ATT 不根据 `Content-Type` 改变或猜测格式。

### 3.1 非流式 JSON

Chat Completions 要求：

- `choices` 中恰有一个数值 `index == 0`；
- 该 choice 的 `finish_reason` 是字符串；优先使用字符串 `message.content`，content 缺失或
  为 null、但有字符串 `message.refusal` 时使用 refusal 并映射到 ContentFilter，两者都没有时
  失败；任一字段存在但不是字符串或 null 时失败。

`finish_reason` 的 `stop`、`length`、`content_filter` 分别映射到统一的 Stop、Length、
ContentFilter，其他字符串保留为 Other。

Responses 要求：

- `status` 是 `completed` 或 `incomplete`；
- `output` 是数组；ATT 按顺序查找其中 `type = "message"`、`role = "assistant"` 的消息，
  再按顺序连接全部 `type = "output_text"` 的字符串 `text`；没有 output text、但存在
  `type = "refusal"` 时，连接其字符串 `refusal` 并映射到 ContentFilter；
- `completed` 映射到 Stop；`incomplete` 必须提供字符串
  `incomplete_details.reason`，其中 `max_output_tokens` 与 `content_filter` 分别映射到
  Length 与 ContentFilter，其他字符串保留为 Other。`incomplete` 即使尚未产生 output text
  也保留为空正文的统一响应，交给翻译响应验收处理；`completed` 必须含 output text 或 refusal。

Chat Completions 的其他 choice、Responses 的 reasoning 等其他 output item，以及两个协议
未消费的 `id`、`usage` 和供应商扩展字段都忽略。拿到统一 Assistant 正文后，对应引擎继续
执行完整 ID、value 形状、ATT token、语言和逐 ID 验收。

### 3.2 流式 SSE

SSE 使用空行分隔事件，接受 LF、CRLF 或 CR 行尾；网络 chunk 和 UTF-8 字符可以跨读取边界。
注释与没有 `data` 的心跳忽略，同一事件的多条 `data` 以 LF 连接。连接结束时，尚未由空行
分派的残留事件不补发；缺少协议终态的正常 EOF 是无效响应。

Chat Completions 的每条 `data`（终止标记除外）必须是 JSON：

- 顶层 `error` 或 `type = "error"` 立即作为无效成功响应失败；
- `choices = []` 的事件忽略（包括 usage）；每个其他事件至多接受一个数值 `index = 0` 的 choice，
  没有时忽略，重复时失败。`delta.content` 和 `delta.refusal` 只接受字符串、null 或缺失；ATT
  按顺序连接 content 字符串，只有没有 content 字符串而存在 refusal 字符串时才连接 refusal
  并映射到 ContentFilter；
- `finish_reason` 按 3.1 节的同一规则映射。取得该字符串及正文后，独立的 `[DONE]` 立即
  结束响应；如果供应商省略 `[DONE]`，只有 HTTP body 正常完整结束、SSE decoder 没有残留
  半个事件时，才以同一个明确 finish 建立响应。第二个 finish reason、finish reason 后再次
  出现 `index = 0`、缺少正文或 finish、终态字段类型错误时失败；传输截断永远不能借正常
  EOF 规则成为成功。

Responses 的每条 `data` 必须是带字符串 `type` 的 JSON event：

- SSE 显式 `event` 为 `response.*` 或 `error` 时，必须与 JSON `type` 完全相同；
- `response.completed` 与 `response.incomplete` 是唯一成功终态。ATT 读取其中的 `response`
  object，要求内层 `status` 与事件类型一致，再按 3.1 节同一 Responses 信封规则建立正文和
  finish reason；
- `response.failed` 或 `error` 立即失败，其他增量事件只用于推进传输并忽略；Responses 不接受
  `[DONE]`。

只有完整终态建立的统一 Assistant 正文才交给翻译域验收和任务记录。SSE envelope、心跳、
中间片段和未完整结束的正文不提交、不记录，也不作为 Partial 候选保存。

## 4. 失败与重试

Retryable 包含 DNS/连接/发送/读取（包括 SSE chunk）/完整请求超时或中断，HTTP 408、429、500、
502、503、504，以及 HTTP 200 流式响应中无法形成完整协议终态的 JSON、SSE 事件或成功信封。
后者不接受或保存任何半截正文，只重新执行完整模型请求。`Retry-After` 支持秒数和 HTTP-date，
并受 Client `max_retry_after_ms` 约束。Translate 按 Client `retry_delays_ms` 执行有限重试；
本地等待不消耗重试次数。普通 429 的 `Retry-After` 对同一 Client 的所有请求生效：等待结束前，
其他 worker 也不能发出下一次请求。

运行根把 HTTP 状态、供应商稳定 code/type、标准 `error.message`、`Retry-After` 和类型化
失败事实交给调用方，让多次逻辑 attempt 能汇总到同一条任务记录。服务状态只根据 HTTP
状态和允许识别的供应商 code/type 分类，绝不解析 `error.message` 或其他错误正文猜测认证、
额度或账户状态。三个供应商字段彼此
独立：其中一个缺失或类型错误，不会抹掉另外两个合法字符串。只读取顶层
`error` 对象，不猜测顶层 `message`、`detail`、`error_description` 或纯文本正文。
原始 Header 和任意非 200 wire body 留在运行根内部。

attempt 只在真实外部 HTTP 发送开始时计数。等待许可、限速、请求构造失败、发送前取消和
已关闭准入门的拒绝都是零 attempt；调用方不得据此记录 Task started 或补造模型证据。

请求层和 Translate 调度层共享同一次运行的停止事实：

- HTTP 401、403，或允许识别的永久额度、账户错误一经确认，就停止后续请求和 Task
  准入；本次 Translate 为 Failed，退出码为 `1`；
- 普通 429 等待超过 `max_retry_after_ms` 或重试耗尽时，当前 Task 为 Unavailable，并停止
  后续请求和 Task 准入；Translate 为 Incomplete，退出码仍为 `0`；
- DNS、连接、读取、超时、HTTP 500 等普通可重试问题耗尽时，只有当前 Task 为
  Unavailable，后续 Task 仍可继续；
- 外部请求失败确认前已经准入且获得有效响应的 Task 仍按自然顺序验收，并在当前 CAS 仍成立时
  提交；失败 Task 自身不提交，失败后不再补充新的 Task。数据库提交、内部验收或取消失败仍按
  各自状态边界决定是否允许后续副作用。

任务准入满足 `planned = started + not_started`。服务停止后没有开始的任务计入
`not_started`；不得把请求门拒绝误计为一次已开始任务，也不得补造完成进度。

Fatal 包含请求构造失败、TLS/证书问题、其他不可重试 HTTP 状态，以及非流式 200 响应的无效
JSON 或成功信封。流式 200 的事件 JSON、核心字段、错误事件和终态问题进入上述有限重试；预算
耗尽后该 Task 为 Unavailable，不把已经收到的增量片段交给调用方。非 200 状态诊断会说明 HTTP
状态、`Retry-After`、供应商 code/type，以及标准 `error` 信封中经过闭集替换和单行清理的
`message`；流式失败区分事件 JSON、服务错误事件、缺少明确终态或其他信封违反，但不回显事件
正文。完整错误正文不落盘。

内部错误区分 DNS、连接、发送、读取、TLS、timeout、HTTP status、响应 JSON 和成功信封
错误。进入 CLI、项目日志和任务记录前，只呈现 Endpoint 对象、直接原因、状态影响和处理办法；必要
时附 HTTP 状态、Retry-After、供应商 code/type 或 JSON 行列。Endpoint 只公开 scheme、host
和可选 port，不记录 path、query、凭据或供应商请求 ID，也不从后端正文解析内部状态。

## 5. 生命周期

shutdown 关上入口：新请求不再开始，尚未进入 HTTP 的等待者被唤醒；已经在 HTTP
中的完整 JSON 或 SSE 请求继续走到明确终态。无论走哪条路径，活动许可都通过 RAII 如数归还。

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
