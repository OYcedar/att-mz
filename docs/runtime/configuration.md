# ATT 配置现行规格

除 Help 和 Version 外，每次进程都通过顶层 `--config FILE` 指定 UTF-8 TOML。相对配置
路径以当前工作目录为基准；配置内路径以配置文件所在目录为基准。

当前顶层只允许：

- `[projects]`
- `[prompts]`
- `[llm]`
- `[[languages]]`
- `[translation]` 与 `[[translation.profiles]]`

未知字段、重复 key、错误类型、空白 ID 和规范化后的重复 ID 都严格拒绝。配置只解析当前
命令实际使用的子树。

## 1. 项目路径

```toml
[projects]
root = "projects"
```

工作区固定为：

```text
<projects.root>/<mv|mz|generic>/<project-name>/
```

项目租约、数据库、日志、任务记录、候选和输出位置不能另行配置。

## 2. Prompt 与翻译 Profile

```toml
[prompts]
root = "prompts"
locale = "auto"
thinking_output = false

[translation]
record_translation_tasks = false

[[translation.profiles]]
id = "primary"
llm_client = "primary"
target_task_user_message_characters = 24000
```

- Prompt 三个字段都必填；
- `record_translation_tasks` 可省略，默认 `false`；
- Profile 的 `id` 和 `llm_client` 必须非空；
- `llm_client` 必须引用现有 Client；
- `target_task_user_message_characters` 是正整数，是 TaskBlock 的目标而非硬上限；
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
quote_repair_pairs = [["“", "”"], ["‘", "’"]]
```

每种语言类型只接受自己声明的字段。Translate 校验全部定义，再精确选择项目源语言模块。
完整语义见[语言规格](../translation/language.md)。

## 4. LLM Client

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

`rate_limit` 整表可省略。存在时两个值都必须为正。`proxy` 是 `false` 或不含凭据的 URL。
`parameters` 必须是严格 JSON object，顶层不得包含 `model`、`messages` 或 `stream`。
ATT 不展开 `api_key` 环境变量。

超时、重试、代理、PEM 和 rate limit 都是外部服务约束。内部 worker、TaskBlock 数量、
SQLite 策略和文件总量不是配置。

## 5. 路径与敏感信息

| 路径 | 相对基准 |
|---|---|
| `--config FILE` | 进程当前工作目录 |
| `projects.root`、`prompts.root`、`additional_pem_files` | 配置文件所在目录 |
| CLI 的游戏、JSONL、Rules、术语、Placeholder 与 Lua 路径 | 进程当前工作目录 |

配置诊断展示路径、字段、一基行列和具体原因，但按照
[Chat Completions 规格](chat-completions.md#6-敏感信息闭集唯一权威)处理敏感值。
