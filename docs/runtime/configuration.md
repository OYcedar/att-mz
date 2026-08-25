# ATT 配置现行规格

除 Help、Version 和 CLI 语法错误外，每次进程都读取实际运行的 `att.exe` 同目录下唯一的
UTF-8 `config.toml`。该文件缺失、不可读或无效时，命令明确失败；
CLI 不接受自定义配置路径，也不搜索当前工作目录、环境变量或其他候选位置。

当前顶层只允许：

- `[prompts]`
- `[llm]`
- `[[languages]]`
- `[translation]` 与 `[[translation.profiles]]`
- `[write_back]`

未知字段、重复 key、错误类型、空白 ID 和规范化后的重复 ID 都会被严格拒绝——启动
时说清楚，胜过带着歧义往下走。配置只解析当前命令实际使用的子树。

## 1. 固定发行目录

`att.exe` 所在目录是发行根，配置、项目和 Prompt 的位置固定为：

```text
<att-dir>/config.toml
<att-dir>/projects/<mv|mz|generic>/<project-name>/
<att-dir>/prompts/
```

`projects/` 和 `prompts/` 不是配置项。项目租约、数据库、日志、任务记录、候选和输出的
位置都由项目工作区固定；命令实际需要某个目录时再按对应规格创建或报告错误，启动阶段
不做额外预检。

## 2. Prompt 与翻译 Profile

```toml
[prompts]
thinking_output = true
source_echo = false

[translation]
record_translation_tasks = true

[[translation.profiles]]
id = "primary"
llm_client = "primary"
target_task_user_message_characters = 24000
```

- Prompt 的 `thinking_output` 与 `source_echo` 都必填，分别控制可读翻译思考和原文回显；
  两个开关互不排斥，可以同时开启；
- 两个开关及其实际选择的 Prompt 内容都进入自动译文状态；改变任一开关会使受影响的
  自动译文不再是 Current；
- `record_translation_tasks` 可省略，默认 `true`；只有操作者明确不需要可读的模型任务记录时才设为 `false`；
- Profile 的 `id` 和 `llm_client` 必须非空；
- `llm_client` 必须引用现有 Client；
- `target_task_user_message_characters` 是正整数，是完整原文稳定投影的 TaskBlock 装箱目标，
  不是最终 user message 的硬上限；译文、术语、Placeholder token 和临时 ID 不参与装箱；
- MV、MZ 和 Generic 共用 Profile 定义，但每个项目分别保存最近采用的 ID。

Prompt 文件和模型协议见[Prompt 规格](../translation/prompts.md)，任务记录见
[任务记录规格](../translation/task-records.md)。

## 3. 语言

```toml
[[languages]]
type = "japanese"
id = "ja"
minimum_kana_characters = 1
allowed_terms = []

[[languages]]
type = "english"
id = "en"
minimum_word_count = 1
minimum_letter_count = 2
ignored_terms = []
minimum_copied_word_count = 2
minimum_copied_letter_count = 4
allowed_terms = ["Page Up", "Page Down"]
```

每种语言类型只接受自己声明的字段。Translate 校验全部定义，再精确选择项目源语言
模块。英语的 `ignored_terms`
只改变译前准入；`allowed_terms` 只允许译文保留已经确认的英文项，不改变译前判断或临时
ID 分配。WriteBack 不读取语言配置；它只按本规格的独立正文开关处理当前译文。完整语言
语义见[语言规格](../translation/language.md)。

## 4. WriteBack 正文开关

```toml
[write_back]
repair_punctuation = true
complete_continuation_whitespace = true
```

两个字段都可省略并默认 `true`，也可以独立设置。整个 `[write_back]` 表省略时同样使用
这两个正式默认。

- `repair_punctuation` 只处理自动译文：开启后，将译文中已经存在、并能与原文唯一对应的
  标点替换为原文实际字符；不插入、删除、移动标点，不复制原文空白，也不改 Placeholder、
  RPG Maker 控制符或 Rules Literal。处理范围包括引号、括号、词内撇号、逗号、句号、
  冒号、分号、问号、叹号、省略号、破折号类、正反斜杠及可唯一归入同类的 Unicode 标点；
  歧义位置保持不变。人工译文始终跳过。关闭后不会因标点修复改变译文；独立开启的排版
  或补空白仍可执行自己的修改；
- `complete_continuation_whitespace` 对人工和自动译文都生效。开启后，为未闭合成对符号内
  需要缩进的续行补一个 U+3000 全角空格，已有半角、全角或 NBSP 行首空白时不重复；
  RPG Maker 控制符保留在空格之前。它不依赖排版规则，也不改变 Choice 固定空槽校验。

规则驱动的自动断行不使用配置中的统一宽度。宽度与精确位置只由本次或项目已保存的
[WriteBack 排版规则](../translation/write-back-layout-rules.md)决定。

## 5. LLM Client

下面是发行模板采用的高吞吐配置。模型服务确有更低并发、限速、代理、证书或超时限制时，
操作者按该服务的实际限制调整对应字段；这些外部限制不改变 ATT 的翻译验收和持久化语义。

```toml
[llm.clients.primary]
protocol = "chat_completions"
url = "https://api.example.com/v1"
api_key = "replace-with-api-key"
model = "replace-with-model-id"
max_concurrent_requests = 16
connect_timeout_ms = 5000
read_timeout_ms = 120000
request_timeout_ms = 120000
proxy = false
additional_pem_files = []
retry_delays_ms = []
max_retry_after_ms = 1000
parameters = '''
{}
'''
```

`protocol` 可省略并默认 `chat_completions`；使用 Responses 时显式设为 `responses`。ATT
只按该字段选择请求和响应协议，不从 URL 或模型名猜测。`url` 可以是包含供应商路径前缀的
基础地址，也可以是已经以 `/chat/completions` 或 `/responses` 结尾的完整端点；ATT 会保留
路径前缀和 query，并把已知后缀规范化为所选协议的路径。它不自行插入 `/v1` 等供应商版本路径。

`rate_limit` 整表可省略；一旦给出，两个值都必须为正。`proxy` 取 `false` 或一个
不含凭据的 URL。`parameters` 是严格 JSON object，顶层留给 ATT 的 `model`、`stream`
及所选协议的 `messages` 或 `input` 不出现在这里；Responses 的 `background` 也由 ATT
固定为 `false`，不接受后台任务。`api_key` 按字面读取，ATT 不展开环境变量。

发行模板不启用 `rate_limit`。只有模型服务确实规定 RPM 时才增加：

```toml
[llm.clients.primary.rate_limit]
requests_per_minute = 60
burst = 8
```

超时、重试、代理、PEM 和 rate limit 描述的是外部服务约束，所以进入配置；内部
worker、TaskBlock 数量、SQLite 策略和文件总量由执行代码决定，不出现在配置里。

## 6. 路径与敏感信息

| 路径 | 相对基准 |
|---|---|
| `config.toml`、`projects/`、`prompts/` | 实际运行的 `att.exe` 所在目录 |
| `additional_pem_files` | 实际运行的 `att.exe` 所在目录 |
| CLI 的游戏、JSONL、Rules、术语、Placeholder、WriteBack 排版规则与 Lua 路径 | 进程当前工作目录 |

配置出错时，诊断会给出路径、字段、一基行列和具体原因；敏感值按
[OpenAI-compatible HTTP 规格](openai-compatible.md#6-敏感信息闭集唯一权威)处理。
