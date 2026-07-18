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
| `runtime.filesystem.tree` | 来源指纹、候选树和候选编辑共同使用的树条目、深度、总字节与单文件字节上限 |
| `runtime.filesystem.publisher` | 同时存活的暂存候选数、单目标恢复产物数和目录发布锁等待上限 |
| `runtime.filesystem.project_lock` | 同项目四命令跨进程租约的等待上限 |
| `runtime.sqlite` | 短操作线程、连接总预算、交互会话、SQL/参数/结果上限和 SQLite 持久策略 |
| `runtime.llm` | HTTP 连接池、总准入、活动请求、等待时限、代理和 TLS 根 |
| `runtime.lua` | Lua VM 线程、队列、栈、单 VM 内存和取消检查周期 |
| `runtime.lua.host_values` | Lua Host 值在 JSON、来源、MZ 与候选文件门面中的总字节、节点数和嵌套深度上限 |

`runtime.llm.proxy` 只接受 `false` 或一个显式 URL；程序不读取系统代理。`runtime.llm.tls.additional_pem_files` 是额外 PEM 根证书文件列表，Windows 原生根仍然使用。

`runtime.filesystem.project_lock.timeout_ms` 是项目租约的必填非零等待时间。租约文件
固定放在 `<projects.root>/.att-project-locks/`；同一项目超时映射为 `ProjectBusy`，
业务模块不另行推断或重试。

`runtime.filesystem.tree` 的 `max_entries`、`max_depth`、`max_bytes` 和
`max_single_file_bytes` 全部必填且必须非零；单文件上限不得大于树总字节上限。
来源指纹、目录候选复制与复核以及 WriteBack 候选编辑使用同一份受信树预算。

`runtime.lua.host_values` 的 `max_bytes`、`max_nodes` 和 `max_depth` 全部必填且必须
非零。该预算统一限制 Lua 与 Host 交换的结构化值，不由 Runtime 根据脚本或输入规模
另行推断。

SQLite `journal_mode` 只允许 `delete`、`truncate`、`persist`、`wal`；`synchronous` 只允许 `normal`、`full`、`extra`。这些选择作用于建库、短操作和交互会话的共享生产策略。

## 3. 公共 LLM 客户端配置

`llm.clients` 是全产品共享的 OpenAI-compatible Chat Completions 客户端目录，
必须至少包含一项。每个客户端的 `id` 非空白、无首尾空白、区分大小写且在
目录中唯一；具体游戏引擎的翻译配置只按这个精确 ID 引用客户端，不重复拥有
endpoint、凭据或模型参数。

每个客户端显式建立 endpoint、认证、model、单请求超时、请求/成功响应/错误响应
三类字节上限以及 RPM/burst。endpoint 必须为不含 fragment 或内嵌凭据的 HTTPS
URL；只有同时设置 `allow_plain_http_loopback = true` 且主机是 loopback 时才允许
HTTP。`model` 必须是不含首尾空白的非空精确标识。

`auth` 必填且只有两种当前写法：

```toml
auth = "none"
auth = { bearer = "replace-with-api-key" }
```

Bearer 必须非空、无首尾空白并能原样构造 HTTP Authorization Header。配置边界
立即把它转换为秘密值；配置、客户端、错误、日志及进程输出的 `Display` 和
`Debug` 都不得暴露密钥。

`request_body_extra` 同样必填，是一个以 TOML 字符串承载的完整 JSON 对象；没有
扩展参数时显式写 `'''{}'''`。JSON 递归拒绝重复键，并拒绝注释、尾逗号、并列值、
截断内容和非对象顶层。顶层不得包含 `model`、`messages` 或 `stream`，嵌套同名键
合法。`n`、`max_tokens`、`max_completion_tokens` 和供应商私有字段均作为用户
拥有的 JSON 值原样进入请求语义，程序不另行解释、补全或改写。

配置解析错误只报告配置路径、安全原因及可用的一基行列；TOML 和 JSON 原文、
Bearer、扩展字段值以及完整配置源码都不进入错误对象或错误链。读取配置使用的
字节、UTF-8 文本和 JSON 临时文本在边界完成后被清零。

## 4. MZ 业务配置

`mz.document`、`mz.standard_asset`、`mz.extract.builtin`、`mz.extract.rules`、`mz.extract.store` 和 `mz.translate.store` 建立各个非根算法的并发与工作粒度。业务模块只执行这些已验证的值，不根据硬件或输入规模另行推断。

`mz.languages` 是语言模块列表。每个 `id` 在列表中唯一。当前只有两种精确类型：

- `type = "japanese"`：假名门槛、允许术语和显式引号修复对；`quote_repair_pairs = []` 表示关闭；
- `type = "english"`：英文词/字母门槛、忽略术语、复制残留门槛和允许术语。

`mz.translation_profiles` 按唯一 `id` 索引。每个 Profile 完整拥有 MZ 的任务并发、
规划和网络重试时间表，并以必填 `llm_client` 精确引用公共客户端目录中的一项；
未知引用在配置边界失败。`planning.systems` 用 `source_language + target_language`
精确选择提示词 Markdown；每个语言对在同一 Profile 中唯一。

## 5. 按命令构造

原始 TOML 每次都完整通过结构和值校验，但环境访问和昂贵资源只为当前命令构造：

| 命令 | 会打开的生产资源 |
|---|---|
| Init | 文件系统、SQLite 建库 |
| Extract | 文件系统、CPU、SQLite；指定 `--lua` 时再构造 Lua |
| Translate | 文件系统、CPU、SQLite、Profile 引用的公共 LLM Client、Delay、Translation Log、Run ID；指定 `--lua` 时再构造 Lua |
| WriteBack | 文件系统、CPU、SQLite、可恢复目录发布、WriteBack Log、Run ID；指定 `--lua` 时再构造 Lua |

因此，Init、Extract 和 WriteBack 虽然同样要求整份 TOML 通过结构和值校验，却不构造
LLM 根、不读取提示词或 TLS PEM，也不发送网络请求。Translate 只为选中 Profile
引用的公共客户端建立运行实例；显式 Lua 与 Standard 共用该实例。
