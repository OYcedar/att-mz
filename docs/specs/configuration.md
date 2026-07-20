# ATT 生产配置规格

## 1. 配置读取、选择与解析阶段

除 Help 和 Version 外，每次 ATT 进程都必须通过顶层 `--config FILE` 指定一个 TOML
配置文件。相对配置路径以进程当前工作目录为基准；配置内部的相对路径以配置文件所在
目录为基准。不存在环境变量、用户目录或其他隐式配置文件回退。

配置源码受 4 MiB 固定上限保护，并且只读取一次。配置边界始终检查 UTF-8、完整 TOML
语法、重复 key 和未知顶层分区，随后仅反序列化当前 CLI 命令实际消费的已知分区：

```text
CLI 命令 + 必填 --config FILE
  ↓
受限读取一次 TOML
  ↓
检查完整语法、重复 key、未知顶层分区
  ↓
选择当前命令所需分区并建立受信类型
  ↓
构造 ConfiguredProductCommand(RpgMakerLayout + ConfiguredRpgMakerCommand)
```

Translate 在这个公共配置阶段完整建立 `prompts.root`、全局语言模块目录、CLI 选中的
RPG Maker Profile 及其引用的公共 LLM Client。打开项目后才取得权威 `LanguagePair`，再执行
第二阶段资源解析：

```text
打开 <projects.root>/<engine>/<project-name>
  ↓
从 metadata 取得受信 LanguagePair
  ↓
按 source LanguageId 精确选择一个共享语言模块
  ↓
读取 <prompts.root>/rpg_maker/<source>--<target>.md
  ↓
构造 ResolvedRpgMakerTranslationResources
```

原始 `toml::Value` 只停留在未受信的 TOML 文档选择边界；`ConfiguredProductCommand`、
`TranslateConfiguration` 及业务模块不得保存延后解释的语言或 Prompt 原始值。已选择
分区严格拒绝缺失字段、未知字段、错误类型、非法值、空白 ID 和重复 ID。已知但未选择
的分区允许缺失，其内部内容不反序列化、不校验、不物化密钥。

用户可修复的配置或资源错误呈现稳定类别以及安全详情：配置路径、可用的一基行列、
字段或资源路径和原因。TOML/JSON 原文、API key、Client parameters、Prompt 内容和完整
配置源码不进入错误对象、错误链或输出。进程输出格式为
`配置或输入错误：<安全详情>`；读取缓冲在配置边界完成后清零。

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
| Translate | `prompts.root`、完整 `languages`、CPU、LLM Runtime、指定 RPG Maker Profile、该 Profile 引用的 Client、标准资产与 Store；只有 `--lua` 才选择 Lua |
| WriteBack | CPU、目录发布与候选编辑、文档和标准资产；只有 `--lua` 才选择 Lua |

Translate 必须解析并验证全部 `[[languages]]` 条目，因此任一非法语言配置或规范化后
重复 ID 都会阻止运行。其他 RPG Maker Profile 和公共 Client 不因存在而被选择；当前 Profile
或 Client ID 重复、引用缺失仍然失败。Profile 通过第一遍只读 ID、第二遍只解析命中
条目的方式选择；未选择 Profile 除 ID 外的内容和未选择 Client 的 API key 都不会被
反序列化或额外物化为秘密值，也不会进入受信配置、Debug、错误链或输出。

四个 `ConfiguredRpgMakerCommand` 变体分别把命令输入与相应受信配置绑定，外层
`ConfiguredProductCommand` 绑定引擎布局；不能把 Translate
配置交给 Init。业务模块不读取配置文件，也不重新解释配置字段。

## 3. 根资源配置与路径

以下配置保留，因为它们拥有当前现实消费者：

| 分区 | 职责 |
|---|---|
| `runtime.async` | Tokio 工作线程、阻塞线程上限和保活时间 |
| `runtime.cpu` | 命令私有 Rayon 池的工作线程选择和有界等待队列 |
| `runtime.filesystem` | 文件工作线程、队列、单文件读取和单目录枚举上限 |
| `runtime.filesystem.tree` | 来源指纹、候选树和候选编辑共用的条目、深度、总字节与单文件预算 |
| `runtime.filesystem.publisher` | 单目标恢复产物数和目录发布锁等待上限 |
| `runtime.filesystem.project_lock` | 同项目四命令跨进程租约等待上限 |
| `runtime.sqlite` | 短操作线程/队列、连接总预算、SQL/参数/查询预算和 SQLite 持久策略 |
| `runtime.llm` | HTTP 连接池、进程内全局并发、有限队列、准入超时、代理和 TLS |
| `runtime.lua` | 每次脚本线程栈、单 VM 内存、取消检查、错误长度和 Host 值预算 |

Lua 每次脚本使用一个专用线程；SQLite 交互命令通道容量固定为 1；每个命令至多持有
一个目录候选。这些是当前产品固定的生命周期事实，不需要用户配置。

CPU 根使用单一现行配置：

```toml
[runtime.cpu]
worker_threads = "auto" # 或正整数
queue_capacity = 64
```

`worker_threads` 只接受精确小写 `"auto"` 或正整数。`auto` 在命令启动时读取进程可用
并行度；探测失败即启动失败。无论自动还是固定值，线程数都显式交给命令私有 Rayon
池，不读取全局 Rayon 池或 `RAYON_NUM_THREADS`。`queue_capacity` 必须大于零；CPU
总准入量等于实际线程数加等待队列容量。所有 RPG Maker 纯 CPU 作业共享该预算，业务
阶段不再拥有重复的解析、扫描、编解码或 scope 并发上限。

SQLite `journal_mode` 只允许 `delete`、`truncate`、`persist`、`wal`；`synchronous`
只允许 `normal`、`full`、`extra`。短操作、建库和唯一交互会话共享这些策略。

工作区固定为 `<projects.root>/<engine>/<project-name>`，其中 `engine` 只能是 `mz | mv`。
项目租约服务选择 `<projects.root>/.att-locks/projects/<engine>/`，目录发布选择
`<projects.root>/.att-locks/directory-publish/<engine>/`；两者均不增加可配置锁根，也不
搜索其他工作区或锁目录。同名 MZ/MV 项目拥有不同工作区和锁命名空间。

不对 `projects.root` 做全局文件系统品牌预检。读取、提取和翻译只要求其实际文件操作
成立；项目租约、目录发布和审计分别在真实操作发生时验证自己需要的锁、身份、同卷
切换、追加和刷盘能力。

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

这些值分别控制唯一审计 worker 的队列、跨进程锁等待、单条记录、活动文件和轮转保留。
审计不是可丢失的调试日志；意图没有持久化时不得开始对应网络请求或目录发布。

## 5. 公共 LLM Client

公共 Client 使用 `url`、`api_key`、`model`、`timeout_ms`、`rpm`、`burst` 和严格 JSON
`parameters`。RPG Maker Profile 只按精确 `llm_client` ID 引用它，不重复拥有网络身份。

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

`parameters` 必须是完整 JSON 对象，递归拒绝重复键，并拒绝注释、尾逗号、并列值和
截断内容。顶层不得包含 `model`、`messages` 或 `stream`；其余字段由用户拥有，程序
不解释或改写。Standard 与 Translate Lua 使用配置边界已经选择的同一个 Client，
共享 HTTP 连接池、全局容量与客户端 RPM/burst；Lua 不接收 RPG Maker planning 或 request
策略。

## 6. 共享语言目录与 RPG Maker Prompt

翻译语言能力属于跨引擎共享配置，使用顶层 `[[languages]]`：

```toml
[prompts]
root = "prompts"

[[languages]]
type = "japanese"
id = "ja"
minimum_kana_characters = 1
allowed_terms = []
quote_repair_pairs = [["“", "”"], ["‘", "’"]]

[[languages]]
type = "english"
id = "en"
minimum_word_count = 1
minimum_letter_count = 2
ignored_terms = []
minimum_copied_word_count = 2
minimum_copied_letter_count = 4
allowed_terms = []
```

`LanguageId` 在 CLI、TOML 和其他外部文本进入内部时执行 RFC 5646 解析、IANA 注册表
校验和 canonicalization。合法大小写变体会立即规范化，例如 `en-us` 成为 `en-US`；
首尾空白、下划线、非法或未注册子标签以及主语言 `und` 均被拒绝。`LanguagePair`
承载规范源语言和目标语言；`LanguageModuleCatalog` 以规范 `LanguageId` 为唯一 key，
精确查询，不做父语言或别名回退。

`prompts.root` 对 Translate 必填。RPG Maker Prompt 不属于共享语言模块，也不属于
Profile；它由 RPG Maker 翻译能力按权威项目语言对派生精确路径：

```text
<prompts.root>/rpg_maker/<source>--<target>.md
```

例如 `ja--zh-Hans.md` 和 `en--zh-Hans.md`。文件名直接使用规范语言标签；只读取该路径
指向的普通文件，不尝试大小写变体、父语言、默认文件或目录首项。文件必须是合法
UTF-8 且内容不能全为空白。系统 Prompt 绑定读取时的精确 `LanguagePair`，并与
同一 `Arc<dyn LanguageModule>` 共同组成 RPG Maker 翻译资源。Prompt
内容和语言策略指纹继续参与语义单元翻译状态指纹。Prompt 作者必须要求模型只翻译带
ID 内容、把无 ID 内容仅作语境、按 ID 到字符串数组的当前 wire 返回、遵守自由断行或
严格对齐约束、精确保留 ATT token，并且不输出说明文字；项目不提供内置 Prompt 正文。

## 7. RPG Maker Profile 与所有权

所有 RPG Maker 算法配置只使用共享的 `[rpg_maker]` 分区：

```toml
[rpg_maker.document]
[rpg_maker.standard_asset]
units_per_decode_job = 32
[rpg_maker.extract.store]
[rpg_maker.translate.store]
units_per_encode_job = 32
```

各表中的必填资源预算由当前实现的受信配置类型定义；缺失时显式失败，不根据引擎、
输入大小或硬件推断默认策略。

```toml
[[rpg_maker.translation_profiles]]
id = "primary"
llm_client = "primary"
max_in_flight_tasks = 4

[rpg_maker.translation_profiles.planning]
max_message_characters = 24000

[rpg_maker.translation_profiles.execution]
network_retry_delays_ms = [500, 1500, 5000]
max_network_retry_after_ms = 30000
```

配置中不存在语言对到 Prompt 的映射。受信 RPG Maker Profile 只保存 ID、非零
任务并发、Planning 配置、Request 配置
和所选公共 Client。Profile ID 精确匹配，不 trim、不折叠大小写、不提供别名或默认项。
所选 Profile 的 `max_in_flight_tasks` 不得超过 `runtime.llm.max_active_requests +
queue_capacity`，且不得使 Standard 的 `2N` 顺序最终化窗口超过运行时 Semaphore 上限。
`runtime.llm` 的活动与排队总容量本身也不得超过该上限；任一组合不满足时都在启动网络
请求前作为配置错误失败。

`LanguageId`、`LanguagePair`、`LanguageModuleCatalog`、公共 LLM、文件、SQLite 和 CPU
执行器、RPG Maker Profile 与 Prompt 协议属于 MV/MZ 共享能力。引擎切片只拥有命令
契约、游戏目录适配和引擎特有投影；项目 schema、数据库对账和翻译资源由共享
RPG Maker 实现拥有。

`rpg_maker.document` 只配置磁盘读取并发；`rpg_maker.standard_asset` 与
Extract/Translate Store 只配置 CPU 作业的批量粒度。实际 CPU 并行预算统一来自
`runtime.cpu`。Init 的五项项目事实来自 CLI 或已有数据库，不从配置推断默认值。首次
创建时五项全部必需；已有项目省略单项表示复用数据库中的当前事实。
