# LLM 调用审阅档案现行规格

## 1. 定位与启用

LLM 调用审阅档案用于人工回答三件事：ATT 实际发送了什么、Provider 实际返回了什么、
ATT 如何处理该响应。它是包含 Prompt、原文、译文和模型正文的敏感证据资产，不是普通
项目日志，也不参与翻译恢复、重放、Current 判断、数据库提交或运行方案持久化。

Translate 必须在 `[llm]` 显式选择是否建立档案：

<!-- att-config-example: fragment -->
```toml
[llm]
record_calls = false
```

该字段没有默认值。Translate 缺失该字段、类型错误或出现未知字段时，在任何 LLM 请求前
失败；Init、Extract、WriteBack 不物化、不校验 `record_calls` 的值，也不要求该字段，
但仍按统一配置边界拒绝未知字段。`record_calls` 对本轮 Standard 和 Translate Lua 的
全部实际调用生效，不属于某个 Client，不提供 CLI 覆盖，也不进入 translation state、
Current、项目数据库或保存运行方案。

关闭时不创建档案目录，也不增加调用记录文件 I/O。开启时，ATT 在任何模型调用前为本轮
RunId 独占创建：

```text
<project-workspace>/llm-calls/<run-id>/
  standard/
    task-000001/
      attempt-001.md
  lua/
    call-000001.md
```

即使本轮没有实际调用，也保留空的 RunId 根目录，证明档案已经开启并通过启动检查。
Standard task、attempt 与 Lua call 都从一开始；零填充只是最低显示宽度，不构成数量
上限。每次 Standard 网络重试使用新的 attempt 文件，每次 `ctx.llm` 调用使用新的 call
文件。目录名只来自受信 RunId、调用种类和数值身份，不使用模型名、Prompt、原文或其他
动态内容。

RunId 与同一命令的普通 JSONL 相同，但档案建立不依赖 JSONL 成功。RunId 目录和调用文件
都使用独占创建，已经存在即失败，绝不共享或覆盖。

## 2. Markdown 内容

每个文件按固定顺序记录以下阶段；面向人的标题、说明与状态使用本轮 UI locale，同时
保留不随 locale 改变的稳定状态 code。文件开头必须警告内容敏感，并说明：如果文件只
完成请求阶段，调用结果未知，不能据此判断 Provider 没有收到请求。

### 2.1 调用归属

归属至少包含 RunId、engine、project、Profile、Client、调用种类、UTC 时间，以及
Standard 的 task/attempt 或 Lua 的 call。数字身份与文件路径一致。

### 2.2 最终有效请求

请求阶段直接来自即将发送的最终请求值，不从模板、规则或内部领域模型反推。它包含：

- 去除查询参数后的 endpoint、Client ID 和 model；
- 合并后的全部非凭据 Client parameters 及 `stream = false`；
- 按实际顺序发送的全部 message role 与完整 content。

API key、`Authorization`、完整请求 Header、代理凭据、TLS 材料和 URL 查询参数永不进入
档案。Client parameters 是实际模型语义，会完整记录；操作者不得把凭据放入
`parameters`。

### 2.3 Provider 结果

Provider 阶段包含请求耗时、HTTP 状态，以及存在时的 `Content-Type`、`x-request-id` 和
`Retry-After`。ATT 不记录其他完整 Header。正文成功解析后，ATT 处理阶段还展示
response ID、finish reason 和 usage，但这些摘要不能替代完整原始正文。

HTTP 成功、非 200、错误信封和畸形 JSON 都保存完整响应正文字节。有效 UTF-8 按原文
展示，并选择长于正文中同字符连续段的 Markdown 围栏，保证正文不能闭合外层结构；非
UTF-8 使用 Base64 无损展示，同时注明原始字节数。发送或连接在完整响应建立前失败时，
以 `response_not_received` 结束 Provider 阶段；取得响应头后读取正文失败时，以
`body_read_failed` 结束，并且不得伪造完整正文。

### 2.4 ATT 处理结果

根信封解析结果以稳定的 `envelope_parse_status = "parsed" | "unavailable"` 明示。
Standard 进一步记录 `Complete`、`Partial` 或 `Unavailable`、接受的 ID，以及每个拒绝
ID 的稳定原因；这表示当前响应已经通过或未通过 ATT 验收，不表示译文已提交数据库。
Lua 只记录响应是否真正越过 native 返回边界，因为脚本而非公共 LLM 根拥有后续解释、
`accept` 和私有事务。成功物化并同步门禁后记录 `delivered_to_lua`；若已解析的响应
无法物化为 Lua 返回值，则记录 `rejected` 与稳定原因 `lua_binding_failed`，不得声称
脚本已经收到响应。

thinking 模式的 `<why>` 正文属于原始 Provider 响应，因此会完整出现在开启的档案中；
它仍不得进入项目数据库、普通 JSONL、终端、安全诊断或 Standard 翻译结果。

## 3. 耐久顺序与失败语义

档案是 LLM 副作用与 ATT 验收的硬门禁。每次调用固定遵守：

1. 取得 Client 活动许可与可选 RPM 许可；
2. 独占创建 Markdown，写完请求阶段并完成 `sync_data`；
3. 发出 HTTP 请求并读取完整响应；
4. 写完 Provider 阶段并完成 `sync_data`；
5. 才解析信封和模型正文；
6. Standard 写完验收结果，或 Lua 写完 `delivered_to_lua`/未交付原因，并再次完成
   `sync_data`；
7. 才允许结果进入 Standard 数据库提交或返回 Lua。

仍在本地等待许可、被取消而未准入或不需要模型的单元不是一次 LLM 调用，不创建调用
文件。请求阶段创建、写入或同步失败时绝不发送请求；Provider 阶段持久化失败时不解析、
不验收、不触发模型重试；ATT 处理阶段持久化失败时不提交当前 Standard 结果，也不把
当前 Lua 响应返回脚本。

发送失败和正文读取失败本身可以按 Client/Standard 既有规则重试，但必须先把当前
attempt 的终态持久化；每次重试生成新文件。Lua 根仍不自动重试，脚本再次调用
`ctx.llm` 才是下一次 call。

任一档案创建、写入或同步失败都会使本轮 Translate 进入不可恢复的技术失败：停止开始
新调用，让已经进入 HTTP 的调用完成自身记录生命周期，并保留此前已经合法提交的
Standard 进度。Lua 的 `pcall` 不能把这种全局证据失败转成命令成功。诊断必须给出阶段、
档案路径、文件操作、OS 稳定代码和状态影响；如果 Provider 可能已经接受请求，还必须
说明调用可能已经发生或产生费用。Provider 错误与档案错误同时存在时分别保留，不能互相
覆盖，也不能把请求或响应正文复制进安全诊断。

档案和 SQLite 之间不建立跨介质事务。Markdown 不记录未经确认的“数据库已提交”；真实
译文和 Current 状态只以 `project.db` 为权威，提交结果与命令终态通过项目状态和普通
JSONL 判断。

## 4. 隐私、保留与非目标

`llm-calls/` 不属于 `logs/`，不继承普通项目日志的脱敏承诺。它可能包含未发布游戏文本、
个人编写的 Prompt、完整模型输出和自定义参数，应将整个项目工作区视为私密材料，避免
提交版本库、上传公共问题或直接分享；项目工作区可以位于仓库外，因此仓库
`.gitignore` 不能替操作者保证隔离。

ATT 不轮转、抽样、截断、压缩、设置大小或数量上限、自动删除这些文件，也不提供可配置
路径。当前实现不增加索引、查看器、数据库表、恢复器、回放器、迁移器或格式版本字段。
目录层级和普通 JSONL 中的安全 RunId/调用身份足以完成当前追溯。
