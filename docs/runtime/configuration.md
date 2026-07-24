# ATT 生产配置现行规格

仓库根目录的 [`config.example.toml`](../../config.example.toml) 是当前版本唯一示例。
配置只表达操作者真正能够选择的路径、Prompt、语言、模型服务和业务 Profile；线程、
队列、批次、SQLite 持久策略、日志缓冲以及文件、Lua、SQLite、Claim、Unit、Group、Task
总量都不是配置项。

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
才解析 Prompt、全部语言、所选 Profile 和该 Profile 引用的 Client。未选 Client 的
密钥不会物化。选中的表严格拒绝缺失、未知、错误类型、空白 ID 和规范化后的重复 ID。

配置错误展示配置路径、一基行列、字段和具体原因；不得回显 API key、Client
`parameters` 值或完整配置正文。

Translate 还会严格读取 `[llm].record_calls`。它是本轮是否保存完整模型输入输出的敏感
审阅选择；没有默认值，也不属于任何 Client。其他命令不物化、不校验该值，也不要求
该字段；统一未知字段检查仍然生效。

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

## 3. LLM Client

模型服务的真实外部约束全部属于 Client：

<!-- att-config-example: fragment -->
```toml
[llm]
record_calls = false

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
等待活动许可或 RPM 只形成背压并响应取消，不产生本地队列满或准入超时错误，也不计为
模型失败或重试。

`record_calls` 必须是布尔值。`false` 不创建调用档案；`true` 为本轮全部 Standard 与
Translate Lua 调用创建独立 Markdown，完整语义见
[LLM 调用审阅档案](llm-call-review.md)。它不写入项目数据库或运行方案，切换它不改变
translation state，也不使 Current 译文失效。

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

[[languages]]
type = "japanese"
id = "ja"
minimum_kana_characters = 1
allowed_terms = []
quote_repair_pairs = [["“", "”"], ["‘", "’"]]

[[rpg_maker.translation_profiles]]
id = "primary"
llm_client = "primary"
max_task_user_message_characters = 24000
```

Profile 只拥有单任务最终 user message 的 Unicode 字符上限和 Client 引用。该上限计算
Planner 实际生成的完整 user message，不包含 system message、请求外壳或模型输出。Profile
不拥有 worker、任务在途数、planning/execution 包装或重试副本。活动 HTTP 宽度来自所选
Client 的 `max_concurrent_requests`（N）；内部完成重排窗口由最大真实样本消融与慢首任务
压力测试确定为 2N，因此总在途宽度为 3N。这些内部执行策略由程序拥有，不进入配置。

`prompts.locale = "auto"` 复用本进程已经解析的 UI locale；显式值按现有 UI i18n 规则
规范化。资源路径固定为：

```text
<prompts.root>/rpg_maker/<locale>/system.md
<prompts.root>/rpg_maker/<locale>/thinking.md
```

`system.md` 始终读取；只有 `thinking_output = true` 才读取 `thinking.md`。资源只按上述
精确路径选择。Translate 验证全部 `[[languages]]`，然后按项目 metadata 的规范
LanguagePair 精确选择源语言模块。

## 5. 程序拥有的运行时策略

下列事实固定由当前实现和真实性能测试拥有：

- Tokio 使用操作系统可用并行度；探测失败明确启动失败；
- CPU 密集型工作使用命令私有 Rayon 池；
- 文件与 SQLite 短操作使用程序选择的有限 worker，饱和时自然等待；
- SQLite 固定使用 WAL + FULL；
- 项目锁、发布锁和 SQLite busy 不设置任意截止时间；
- 日志固定写入项目工作区的 `logs/<run-id>.jsonl`；
- 开启的 LLM 调用审阅档案固定写入项目工作区的 `llm-calls/<run-id>/`；
- 文档、规则、Group、任务和不同物理文件在不破坏确定性的前提下并行；
- 自然顺序、代表选择、提交顺序和最终主错误不受完成顺序影响。

这些策略不形成项目总量上限。ATT 不比较文件字节数、目录项、目录深度、目录总字节、
Lua 值大小或复杂度、SQLite 查询组/行/结果字节以及 Claim、Unit、Group、Task 总数。
规范输入只会因真实文件系统、操作系统、地址空间、内存、SQLite、外部协议或格式错误
失败，并报告实际底层原因。

## 6. 运行方案

运行方案不写入生产配置：Init 来源、Extract owner 集合、Translate Profile 和 WriteBack
Lua 选择属于项目数据库。Rules 保存 canonical 语义；Lua 保存阶段正文快照、指纹和无损
路径。保存的 Profile 在当前配置中不存在时明确失败，不选择其他 Profile。

解析器只实现本规格列出的当前字段和语义；其他内容按普通无效输入处理。
