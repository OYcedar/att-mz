# ATT 生产配置规格

## 1. 配置来源

ATT 每次进程只读取一个 TOML 文件。顶层 `--config FILE` 指定文件时，相对路径以进程当前工作目录为基准；未指定时使用 `%APPDATA%\ATT\config.toml`。配置内部的所有相对文件和目录都以配置文件所在目录为基准。

配置文件在解析前受 4 MiB 固定上限保护。全部表都严格拒绝未知字段、缺失字段、重复 key、错误类型、空白 ID 和重复 ID；每个语义只接受本规格列出的唯一写法。

仓库根目录的 [config.example.toml](../../config.example.toml) 是可解析的完整契约示例，并由自动测试与当前解析器保持一致。

## 2. 根资源配置

`projects.root` 是所有 MZ 工作区的父目录。`observability.root` 是两条 JSONL 日志流的父目录。两者在真实根构造时必须落在本机、非大小写敏感的 NTFS 卷上。

`runtime` 下的每个分区都是对真实有界资源的显式选择：

| 分区 | 职责 |
|---|---|
| `runtime.async` | Tokio 工作线程、阻塞线程上限和保活时间 |
| `runtime.cpu` | CPU 专用线程数和有界队列 |
| `runtime.filesystem` | 文件工作线程、队列、单文件读取和单目录枚举上限 |
| `runtime.filesystem.publisher` | 目录候选、递归复制、恢复产物和同目标锁的资源上限 |
| `runtime.sqlite` | 短操作线程、连接总预算、交互会话、SQL/参数/结果上限和 SQLite 持久策略 |
| `runtime.llm` | HTTP 连接池、总准入、活动请求、等待时限、代理和 TLS 根 |
| `runtime.lua` | Lua VM 线程、队列、栈、单 VM 内存和取消检查周期 |

`runtime.llm.proxy` 只接受 `false` 或一个显式 URL；程序不读取系统代理。`runtime.llm.tls.additional_pem_files` 是额外 PEM 根证书文件列表，Windows 原生根仍然使用。

SQLite `journal_mode` 只允许 `delete`、`truncate`、`persist`、`wal`；`synchronous` 只允许 `normal`、`full`、`extra`。这些选择作用于建库、短操作和交互会话的共享生产策略。

## 3. MZ 业务配置

`mz.document`、`mz.standard_asset`、`mz.extract.builtin`、`mz.extract.rules`、`mz.extract.store` 和 `mz.translate.store` 建立各个非根算法的并发与工作粒度。业务模块只执行这些已验证的值，不根据硬件或输入规模另行推断。

`mz.languages` 是语言模块列表。每个 `id` 在列表中唯一。当前只有两种精确类型：

- `type = "japanese"`：假名门槛、允许术语和显式引号修复对；`quote_repair_pairs = []` 表示关闭；
- `type = "english"`：英文词/字母门槛、忽略术语、复制残留门槛和允许术语。

`mz.translation_profiles` 按唯一 `id` 索引。每个 Profile 完整拥有任务并发、规划、网络重试时间表和一个 OpenAI-compatible Chat Completions 连接。`planning.systems` 用 `source_language + target_language` 精确选择提示词 Markdown；每个语言对在同一 Profile 中唯一。

LLM `endpoint` 必须为不含 fragment 或内嵌凭据的 HTTPS URL。只有同时设置 `allow_plain_http_loopback = true` 且主机是 loopback 时才允许 HTTP。`model` 必须是不含首尾空白的非空精确标识。`auth` 只接受字符串 `"none"` 或严格对象 `{ bearer_environment = "ENV_NAME" }`。API key 只在 Translate 真正选中该 Profile 时从环境变量解析；环境变量缺失、非 Unicode 或值为空白都会失败，错误不回显密钥值。Init、Extract、WriteBack 不因未使用的密钥缺失而失败。

`completion_limit.parameter` 只允许 `max_tokens` 或 `max_completion_tokens`。`request_options` 保留供应商扩展表达力，但不得覆盖 `model`、`messages`、`stream`、`n`、`max_tokens`、`max_completion_tokens`。

## 4. 按命令构造

原始 TOML 每次都完整通过结构和值校验，但环境访问和昂贵资源只为当前命令构造：

| 命令 | 会打开的生产资源 |
|---|---|
| Init | 文件系统、SQLite 建库 |
| Extract | 文件系统、CPU、SQLite；指定 `--lua` 时再构造 Lua |
| Translate | 文件系统、CPU、SQLite、选中 Profile 的 LLM、Delay、Translation Log、Run ID；指定 `--lua` 时再构造 Lua |
| WriteBack | 文件系统、CPU、SQLite、可恢复目录发布、WriteBack Log、Run ID；指定 `--lua` 时再构造 Lua |

因此，一个与当前命令无关的提示词文件、PEM 文件、API key 或 Lua VM 不会被提前访问。
