# ATT 生产配置现行规格

仓库根目录的 [`config.example.toml`](../../config.example.toml) 是当前版本唯一示例。
配置只表达操作者真正能够选择的路径、Prompt、语言、模型服务、业务 Profile 和可读
翻译任务记录开关；线程、队列、批次、SQLite 持久策略、日志缓冲以及文件、Lua、SQLite、
Claim、Unit、Group、Task 总量都不是配置项。

## 1. 读取与严格边界

除 Help 和 Version 外，每次进程都必须通过顶层 `--config FILE` 指定 TOML 文件。相对
配置路径以当前工作目录为基准；配置中的相对路径以配置文件所在目录为基准。ATT 实际
读取完整文件，不按元数据或字节数设置产品上限，然后检查 UTF-8、完整 TOML、重复 key
和未知顶层分区。

当前只接受五个顶层分区：

- `[projects]`；
- `[prompts]`；
- `[llm]`；
- `[[languages]]`；
- `[rpg_maker]`。

只有上述当前分区有效。未知字段严格拒绝，诊断只说明当前字段要求和具体无效原因。

配置只解析本次命令真正消费的已知子树。例如 Init 只需要 `projects.root`；Translate
才解析 Prompt、`rpg_maker.record_translation_tasks`、全部语言、所选 Profile 和该
Profile 引用的 Client。
未选 Client 的密钥不会物化。选中的表严格拒绝缺失、未知、错误类型、空白 ID 和规范化
后的重复 ID。

配置错误展示配置路径、一基行列、字段和具体原因，并采用
[Chat Completions 规格规定的敏感信息边界](chat-completions.md#6-敏感信息闭集唯一权威)。
稳定配置诊断不复制完整 `parameters` 或配置正文，以维持结构化 schema 与可读体积；
该输出边界不构成敏感性分类。语法、缺失字段、未知字段、重复字段、类型不符和值不符是
互斥的结构化失败；类型不符同时展示当前字段契约要求的字符串、整数、布尔、数组或表形态。
字段身份、值形态和位置来自同一遍 TOML 结构索引：索引只解码 key，不解码或保存值正文，
也不依赖解析库面向人类的英文错误文本来推断分类。

## 2. 最小配置与路径

Init 的最小配置是：

<!-- att-config-example: production-minimal-init -->
```toml
[projects]
root = "projects"
```

项目工作区固定派生为：

```text
<projects.root>/<engine>/<project-name>
```

`engine` 只能是 `mv | mz`。项目租约、目录发布锁、日志目录和写回候选位置都由 ATT 在
项目工作区或项目根下派生，不能另行配置。

| 路径来源 | 相对路径基准 |
|---|---|
| `--config FILE` | 进程当前工作目录 |
| `projects.root`、`prompts.root`、`additional_pem_files` | 配置文件所在目录 |
| 其他 CLI 文件或目录参数 | 进程当前工作目录 |

## 3. LLM 与 Client

模型服务的真实外部约束全部属于 Client：

<!-- att-config-example: fragment -->
```toml
[llm.clients.primary]
url = "https://api.example.com/v1/chat/completions"
api_key = "replace-with-api-key"
model = "replace-with-model-id"
max_concurrent_requests = 8
connect_timeout_ms = 15000
read_timeout_ms = 120000
request_timeout_ms = 120000
proxy = false
additional_pem_files = []
retry_delays_ms = [500, 1500, 5000]
max_retry_after_ms = 30000
parameters = '''
{}
'''

[llm.clients.primary.rate_limit]
requests_per_minute = 60
burst = 8
```

`rate_limit` 整表可省略，表示供应商没有已知的本地限速要求。存在时两个值都必须为正。
等待活动许可或 RPM 时，请求会留在本地等待并响应取消，不产生本地队列已满或等待超时
错误，也不计为模型失败或重试。

`proxy` 只能是 `false` 或不含凭据的代理 URL。附加 PEM 文件在配置加载后读取并交给
HTTP Client。`parameters` 必须是完整 JSON 对象，递归拒绝重复键，并且顶层不得包含
`model`、`messages` 或 `stream`。ATT 不展开 `api_key` 环境变量。

连接、连续读取和完整请求超时是网络边界；`retry_delays_ms` 与
`max_retry_after_ms` 是该供应商请求的重试约束。它们不控制本地排队、SQLite 或锁等待。

## 4. Prompt、语言与 Profile

Translate 使用：

<!-- att-config-example: fragment -->
```toml
[prompts]
root = "prompts"
locale = "auto"
thinking_output = false

[rpg_maker]
record_translation_tasks = false

[[languages]]
type = "japanese"
id = "ja"
minimum_kana_characters = 1
allowed_terms = []
quote_repair_pairs = [["“", "”"], ["‘", "’"]]

[[rpg_maker.translation_profiles]]
id = "primary"
llm_client = "primary"
target_task_user_message_characters = 24000
```

`[prompts]` 是 Translate 必需的配置表，只允许三个必填字段：`root` 是路径字符串，
`locale` 是 locale 字符串，`thinking_output` 是布尔值。缺失字段、未知字段或错误类型
都是配置输入错误；相对 `root` 按本文第 2 节规定，从配置文件所在目录解析。配置通过校验后，
locale 如何选择资源、文件需要满足什么条件、system message 如何装配以及模型协议是什么，
由 [Prompt 资源与模型协议现行规格](../rpg-maker/prompts.md)规定。

`record_translation_tasks` 是可省略的布尔值，默认 `false`，只有 Translate 读取。
开启后的记录范围、文件内容、编号、写入失败处理和其他行为，由
[翻译任务记录现行规格](../rpg-maker/task-records.md)规定。

每个 `[[rpg_maker.translation_profiles]]` 必须提供非空 `id`、引用现有 Client 的
`llm_client`，以及正整数 `target_task_user_message_characters`。该字符数如何用于
Standard Group 和 Managed unit 的 TaskBlock 分组，由
[翻译现行规格](../rpg-maker/translation.md#5-任务规划与模型消息)规定。

Lua Managed API 不增加配置字段。它使用哪些现有翻译设置，以及何时需要改用低级 API，
由 [Lua 现行规格](../rpg-maker/lua.md#72-ctxtranslationstranslateopen)规定。内部 worker、
同时处理的任务数量、重排窗口和 checkpoint 事务不是用户配置。

Prompt locale、资源路径、文件读取条件与消息装配只由
[Prompt 资源与模型协议现行规格](../rpg-maker/prompts.md)规定。Translate 验证全部
`[[languages]]`，然后按项目 metadata 的规范 LanguagePair 精确选择源语言模块。

## 5. 不属于配置的运行时行为

线程、worker、内部任务窗口、队列、批次、缓存、SQLite 工作方式、锁等待、日志路径、
任务记录路径和任务完成顺序都不是 `config.toml` 字段。各项当前行为分别由以下文档规定：

- 线程、worker、内部容量和项目总量：[运行时导航](README.md#哪些值不能配置)；
- Translate 的并发和提交顺序：[翻译现行规格](../rpg-maker/translation.md#9-并发提交与任务结果)；
- SQLite 设置和等待方式：[SQLite 现行规格](sqlite.md)；
- 项目日志：[普通项目日志现行规格](project-log.md)；
- 翻译任务记录：[翻译任务记录现行规格](../rpg-maker/task-records.md)；
- 项目锁和目录发布：[目录发布现行规格](directory-publishing.md)。

配置中也没有用于限制文件、目录、Lua、SQLite 结果、Claim、Unit、Group 或 Task 总量的
字段。超出真实文件系统、操作系统、内存、SQLite、外部协议或数据格式能力时，程序报告
实际失败原因。

## 6. 运行方案

Init 来源、Extract owner 集合、Translate Profile 和 WriteBack Lua 选择属于项目中保存的
运行方案，不是生产配置字段。各命令何时保存、复用或清除这些值，以及保存的 Profile
不存在时如何失败，由 [CLI 与运行方案现行规格](cli.md)规定。独立项目 `lua` 命令同样
不会因此增加配置分区。

解析器只实现本规格列出的当前字段和语义；其他内容按普通无效输入处理。
