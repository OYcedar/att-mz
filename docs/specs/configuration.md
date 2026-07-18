# ATT 生产配置规格

## 1. 配置读取与选择

ATT 每次进程只读取一个 TOML 文件。顶层 `--config FILE` 指定文件时，相对路径以进程当前工作目录为基准；未指定时使用 `%APPDATA%\ATT\config.toml`。配置内部的相对路径以配置文件所在目录为基准。

配置源码受 4 MiB 固定上限保护，并且只读取一次。配置边界始终检查 UTF-8、完整 TOML 语法、重复 key 和未知顶层分区；随后根据当前 CLI 命令及其实际选择，仅解析并验证本次会使用的分区：

```text
CLI 命令
  ↓
受限读取一次 TOML
  ↓
检查完整语法、重复 key、未知顶层分区
  ↓
选择当前命令所需分区
  ↓
直接构造 ConfiguredMzCommand 的互斥变体
```

已选择分区严格拒绝缺失字段、未知字段、错误类型、非法值、空白 ID 和重复的当前 ID。
已知但未选择的分区允许缺失，其内部内容不反序列化、不校验、不物化密钥。每个
`ConfiguredMzCommand` 变体只持有当前命令的受信配置，根构造器直接消费这些值。

配置错误只保存配置路径、安全原因及可用的一基行列。TOML/JSON 原文、API key 和完整配置源码不进入错误对象或错误链；读取缓冲在配置边界完成后清零。

## 2. 按命令选择

所有命令都选择：

- `projects.root`；
- `runtime.async`；
- 文件读取、目录树预算和项目锁等待时间；
- SQLite 基础资源与持久策略；
- `observability.root` 与 `observability.audit`。

其余选择如下：

| 命令 | 额外选择 |
|---|---|
| Init | 目录发布、SQLite 建库与数据库快照 |
| Extract | CPU、所选 Builtin/Rules/Store；只有 `--lua` 才选择 Lua 和交互会话 |
| Translate | CPU、LLM Runtime、指定 Profile、该 Profile 引用的 Client、实际语言对、标准资产与 Store；只有 `--lua` 才选择 Lua |
| WriteBack | CPU、目录发布与候选编辑、文档和标准资产；只有 `--lua` 才选择 Lua |

Translate 只验证 CLI 指定的 Profile、它引用的 Client，以及实际项目语言对需要的 system prompt 和语言模块。其他 Profile、Client 或语言条目不阻止本次运行；当前 ID 重复或当前引用缺失仍然失败。完整 TOML 会短暂存在于读取配置所需的零化缓冲中；未选择 Client 的 API key 不会被反序列化或额外物化为秘密值，也不会进入受信配置、Debug、错误链或输出，读取缓冲在配置选择结束后零化。

配置边界直接产生四个互斥的 `ConfiguredMzCommand` 变体，每个变体把命令输入与相应
受信配置绑定，不能把 Translate 配置交给 Init。业务模块直接接收选定 Profile。

## 3. 根资源配置

以下配置保留，因为它们拥有当前现实消费者：

| 分区 | 职责 |
|---|---|
| `runtime.async` | Tokio 工作线程、阻塞线程上限和保活时间 |
| `runtime.cpu` | CPU 专用线程数和有界队列 |
| `runtime.filesystem` | 文件工作线程、队列、单文件读取和单目录枚举上限 |
| `runtime.filesystem.tree` | 来源指纹、候选树和候选编辑共用的条目、深度、总字节与单文件预算 |
| `runtime.filesystem.publisher` | 单目标恢复产物数和目录发布锁等待上限 |
| `runtime.filesystem.project_lock` | 同项目四命令跨进程租约等待上限 |
| `runtime.sqlite` | 短操作线程/队列、连接总预算、SQL/参数/查询预算和 SQLite 持久策略 |
| `runtime.llm` | HTTP 连接池、进程内全局并发、有限队列、准入超时、代理和 TLS |
| `runtime.lua` | 每次脚本线程栈、单 VM 内存、取消检查、错误长度和 Host 值预算 |

Lua 每次脚本使用一个专用线程；SQLite 交互命令通道容量固定为 1；每个命令至多持有
一个目录候选。这些是当前产品固定的生命周期事实，不需要用户配置。

SQLite `journal_mode` 只允许 `delete`、`truncate`、`persist`、`wal`；`synchronous` 只允许 `normal`、`full`、`extra`。短操作、建库和唯一交互会话共享这些策略。

项目锁文件固定由 MZ 项目租约服务映射到 `<projects.root>/.att-locks/projects/`，目录发布锁位于 `<projects.root>/.att-locks/directory-publish/`。不增加可配置锁根。

不对 `projects.root` 做全局文件系统品牌预检。读取、提取和翻译只要求其实际文件操作成立；项目租约、目录发布和审计分别在真实操作发生时验证自己需要的锁、身份、同卷切换、追加和刷盘能力。

## 4. 审计配置

四个命令共用一份强审计账本：

```toml
[observability]
root = "logs"

[observability.audit]
queue_capacity = 256
lock_timeout_ms = 30000
max_record_bytes = 4194304
max_file_bytes = 268435456
retained_rotated_files = 8
```

这些值分别控制唯一审计 worker 的队列、跨进程锁等待、单条记录、活动文件和轮转保留。审计不是可丢失的调试日志；意图没有持久化时不得开始对应网络请求或目录发布。

## 5. 公共 LLM Client

公共 Client 仍使用 `url`、`api_key`、`model`、`timeout_ms`、`rpm`、`burst` 和严格 JSON `parameters`。MZ Profile 只按精确 `llm_client` ID 引用它，不重复拥有网络身份。

```toml
[llm.clients.primary]
url = "https://api.example.com/v1/chat/completions"
api_key = "replace-with-api-key"
model = "model-id"
timeout_ms = 120000
rpm = 60
burst = 8
parameters = '''{}'''
```

`parameters` 必须是完整 JSON 对象，递归拒绝重复键，并拒绝注释、尾逗号、并列值和截断内容。顶层不得包含 `model`、`messages` 或 `stream`；其余字段由用户拥有，程序不解释或改写。Standard 与 Translate Lua 使用配置边界已经选择的同一个 Client 和执行 Profile，共享 HTTP 连接池、全局容量与客户端 RPM/burst。

## 6. MZ 业务配置

`mz.document`、`mz.standard_asset`、Extract/Translate Store 和实际算法并发均由其现实
消费配置建立。语言模块和 Translation Profile 只在 Translate 选择边界使用；业务模块
直接接收受信的语言对和 `Arc<TranslationExecutionProfile>`。

Init 的五项项目事实来自 CLI 或已有数据库，不从配置推断默认值。首次创建时五项全部必需；已有项目省略单项表示复用数据库中的当前事实。
