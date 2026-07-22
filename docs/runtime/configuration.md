# ATT 生产配置现行规格

根据当前命令寻找需要填写的分区、理解路径基准并校准资源值，见
[配置编写与运行能力导航](README.md)。CLI、省略参数和运行方案的完整契约见
[生产运行时与 CLI](cli.md)。

## 1. 配置读取与受信边界

除 Help 和 Version 外，每次 ATT 进程都必须通过顶层 `--config FILE` 指定一个 TOML
配置文件。相对配置路径以进程当前工作目录为基准；配置内部的相对路径以配置文件所在
目录为基准。不存在默认配置路径、用户目录配置或配置环境变量插值。

`--ui-language`、`ATT_UI_LANGUAGE` 和 `--progress` 是进程界面选择，不属于 TOML，也不
写入项目数据库。已经解析出的有效 UI locale 还会供 `prompts.locale = "auto"` 直接复用，
但不会因此成为项目语言对或持久状态。UI locale 的检测和支持范围见
[CLI 现行规格](cli.md#2-ui-语言)。

配置源码受 4 MiB 固定上限保护，并且只读取一次。配置边界始终检查 UTF-8、完整 TOML
语法、重复 key 和未知顶层分区。随后结合 CLI 意图与项目保存方案，只选择本次命令真实
消费的已知分区：

```text
CLI 命令 + 必填 --config FILE
  ↓
受限读取一次 TOML，检查完整语法与未知顶层分区
  ↓
建立通用配置、项目路径、租约和项目日志配置
  ↓
读取项目 metadata 与本命令保存方案
  ↓
把显式输入、项目状态或固定产品行为解析为完整运行方案
  ↓
选择本方案使用的 Profile、Client、Lua 与纵向配置并建立受信类型
```

已选择分区严格拒绝缺失字段、未知字段、错误类型、非法值、空白 ID 和重复 ID。已知但
未选择的分区允许缺失，其内部内容不反序列化、不校验、不物化密钥。原始 `toml::Value`
只停留在选择边界；业务模块不得保存它并延后解释。

用户可修复的配置或资源错误呈现稳定类别以及安全详情：配置路径、可用的一基行列、字段
或资源路径和原因。TOML/JSON 原文、API key、Client parameters、Prompt 内容和完整配置
源码不进入错误对象、内部来源链、终端或项目日志。读取缓冲在配置边界完成后清零。

## 2. 按命令选择

所有普通命令都选择：

- `projects.root`；
- `runtime.async`；
- 文件读取、目录树预算和项目锁等待时间；
- SQLite 基础资源与持久策略；
- `observability.root` 与 `observability.log`。

其余选择如下：

| 命令 | 额外选择 |
|---|---|
| Init | 目录发布、SQLite 建库与数据库快照 |
| Extract | CPU、文档、Store，以及解析后的 Builtin/Rules owner；本次方案启用 Lua 时选择 `runtime.lua` 和交互会话 |
| Translate | 完整 `[prompts]`、完整 `languages`、CPU、LLM Runtime、解析后的 RPG Maker Profile、该 Profile 引用的 Client、标准资产与 Store；本次方案启用 Lua 时再选择 `runtime.lua` |
| WriteBack | CPU、目录发布与候选编辑、文档和标准资产；本次方案启用 Lua 时选择 `runtime.lua` |

“本次方案启用 Lua”既包括显式非空 `--lua`，也包括从相应阶段数据库快照自动复用。
配置中存在 `runtime.lua` 不会自行启用程序；零字节 Lua 显式清除阶段程序，本次不选择
或执行 Lua。

Translate 的 `PROFILE_ID` 可以来自本次显式输入或项目保存方案。选定 ID 后才以两遍选择
精确解析对应 `[[rpg_maker.translation_profiles]]` 和它引用的 `[llm.clients.<id>]`。保存的
Profile 在当前配置中不存在时是输入错误，不自动选择其他 Profile。未选择 Profile 除 ID
外的内容和未选择 Client 的 API key 不物化为受信值。

Translate 需要解析并验证全部 `[[languages]]` 条目；任一非法语言配置或规范化后重复 ID
都会阻止运行。项目开启后取得权威 `LanguagePair`，再执行第二阶段资源解析：

```text
从 [prompts].locale 与本进程有效 UI locale 取得规范 Prompt locale
  ↓
从 metadata 取得受信 LanguagePair，按 source LanguageId 精确选择共享语言模块
  ↓
读取并渲染 <prompts.root>/rpg_maker/<locale>/system.md
  ↓ thinking_output = true 时
读取同 locale 的 thinking.md，并用两个 LF 装配
  ↓
构造 ResolvedRpgMakerTranslationResources
```

四个命令分别把已解析运行方案与相应受信配置绑定；不能把 Translate 配置交给 Init。
业务模块不读取配置文件，也不重新解释配置字段。

## 3. 根资源配置与路径

以下配置拥有当前现实消费者：

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

Lua 每次脚本使用一个专用线程；SQLite 交互命令通道容量固定为 1；每个命令至多持有一个
目录候选。这些是当前产品固定的生命周期事实，不需要用户配置。

CPU 根使用单一现行配置：

```toml
[runtime.cpu]
worker_threads = "auto" # 或正整数
queue_capacity = 64
```

`worker_threads` 只接受精确小写 `"auto"` 或正整数。`auto` 在命令启动时读取进程可用
并行度；探测失败即启动失败。无论自动还是固定值，线程数都显式交给命令私有 Rayon 池，
不读取全局 Rayon 池或 `RAYON_NUM_THREADS`。`queue_capacity` 必须大于零；总准入量等于
实际线程数加等待队列容量。全部 RPG Maker 纯 CPU 作业共享该预算。

SQLite `journal_mode` 只允许 `delete`、`truncate`、`persist`、`wal`；`synchronous` 只
允许 `normal`、`full`、`extra`。短操作、建库、运行方案事务和唯一交互会话共享策略。

工作区固定为 `<projects.root>/<engine>/<project-name>`，其中 `engine` 只能是 `mz | mv`。
项目租约位于 `<projects.root>/.att-locks/projects/<engine>/`，目录发布锁位于
`<projects.root>/.att-locks/directory-publish/<engine>/`。同名 MZ/MV 项目拥有不同工作区
和锁命名空间，不搜索其他工作区或锁目录。

不对 `projects.root` 做全局文件系统品牌预检。读取、提取和翻译只要求真实文件操作成立；
项目租约、目录发布和项目日志分别在实际操作时验证自己需要的锁、身份、同卷切换或追加
能力。日志能力验证失败只影响日志健康，不能反向改变业务结果。

## 4. 项目日志配置

四个命令共用一份不可失败的普通项目日志：

```toml
[observability]
root = "logs"

[observability.log]
level = "info"
queue_capacity = 1024
batch_max_records = 64
batch_max_bytes = 1048576
flush_interval_ms = 100
shutdown_timeout_ms = 2000
lock_timeout_ms = 1000
max_record_bytes = 262144
max_file_bytes = 67108864
retained_rotated_files = 4
```

所有字段必填。`level` 只接受 `error | warn | info | debug`；容量、字节、间隔、超时和
保留数量由配置边界完成组合校验。配置本身无效是输入错误；配置成功后，日志启动、队列、
锁、写入、轮转、保留和关闭故障最多警告一次，不停止业务也不改变退出码。

`root` 的相对路径以配置文件目录为基准。文件布局、JSONL 字段、批处理和安全边界见
[普通项目日志](project-log.md)。

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

`parameters` 必须是完整 JSON 对象，递归拒绝重复键，并拒绝注释、尾逗号、并列值和截断
内容。顶层不得包含 `model`、`messages` 或 `stream`；其余字段由用户拥有，程序不解释或
改写。Standard 与 Translate Lua 使用同一个已选 Client，共享 HTTP 连接池、全局容量与
客户端 RPM/burst；Lua 不接收 RPG Maker planning 或 request 策略。

`api_key` 是配置中的实际字符串，当前不会展开环境变量。`"$NAME"` 只表示字面值；真实
凭据应放在不纳入版本控制且访问受限的本地配置中。代理 URL 不得内嵌凭据。

## 6. 共享语言目录与 RPG Maker Prompt i18n

翻译语言能力属于进程级共享配置，使用顶层 `[[languages]]`：

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

游戏翻译的开放 `LanguageId` 与终端闭集 `UiLocale` 是两个独立类型。`LanguageId` 在 CLI、
TOML 和其他外部文本进入内部时执行 RFC 5646 解析、IANA 注册表校验和 canonicalization；
精确查询时不做父语言或别名回退。

`prompts.root`、`prompts.locale` 与 `prompts.thinking_output` 都是 Translate 必填字段；
该表不允许其他字段，缺失、未知或错误类型都是配置输入错误。非 Translate 命令不反序列化
或校验 `[prompts]` 内部字段。`locale` 接受精确小写 `auto`，
或能按现有 UI i18n 规则映射到受支持语言的有效 BCP-47 locale；例如 `fr-CA` 规范为 `fr`，
`zh-TW` 规范为 `zh-Hant`。显式值优先；`auto` 复用本进程已经解析完成的有效 UI locale，
不再次读取 `--ui-language`、环境变量或 Windows 用户语言。资源路径只使用以下规范标签：

```text
ar  zh-Hans  zh-Hant  en  fr  ru  es  ja  ko  vi
```

Prompt 资源按规范 Prompt locale 派生唯一路径：

```text
<prompts.root>/rpg_maker/<locale>/system.md
<prompts.root>/rpg_maker/<locale>/thinking.md
```

`system.md` 始终读取；`thinking_output = false` 时完全不读取 `thinking.md`，开启时才
读取同一 locale 的两份资源。每个被选择的资源都必须是普通 UTF-8 非空白文件。没有父
语言、中文、英文、目录首项、大小写变体或旧语言对文件回退；未选择 locale 的资源和
关闭模式下的 `thinking.md` 不影响运行。每次 Translate 重新读取所选文件，不做长期缓存。

`system.md` 去除首尾空白后只允许 `{{source_language}}` 与 `{{target_language}}` 两个
模板变量，两者都必须存在，可以多次出现。ATT 使用项目规范 `LanguageId` 完整替换；
未知、缺失、malformed 或替换后残留的模板变量都使资源无效。`thinking.md` 去除首尾空白
后不得含模板变量。关闭时 system message 只有渲染后的 `system.md`；开启时精确使用
`rendered system.md + "\n\n" + thinking.md`。资源或模板错误在首次 LLM 请求前失败，
用户诊断只包含安全的 locale、组件名和路径以及统一检查方向，不回显正文。

装配后的完整 system Prompt 参与消息字符预算和 translation state。切换 locale、切换
`thinking_output` 或修改本轮实际选择的资源，会自然使受影响旧译文不再 Current。
`thinking_output` 只控制 `<why>` 人工思考信封，不控制 Client `parameters` 中供应商原生
reasoning/thinking 选项。完整模板、响应信封与微调限制见
[系统提示词编写指南](../rpg-maker/prompts.md)。

## 7. RPG Maker 配置与运行方案所有权

RPG Maker 算法配置使用共享的 `[rpg_maker]` 分区：

```toml
[rpg_maker.document]

[rpg_maker.standard_asset]
units_per_decode_job = 32

[rpg_maker.extract.store]

[rpg_maker.translate.store]
units_per_encode_job = 32

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

各表的必填资源预算由受信配置类型定义；缺失时显式失败，不根据版本、输入大小或硬件
推断默认策略。Profile ID 精确匹配，不 trim、不折叠大小写、不提供别名。所选 Profile 的
`max_in_flight_tasks` 必须与 `runtime.llm` 的活动、排队和顺序最终化容量共同通过校验。

运行方案不写入生产配置：

- Init 的上次成功来源路径、Extract 的完整 owner 集合、Translate 的 Profile 和 WriteBack
  的 Lua 启用选择属于项目数据库；
- Extract Rules 保存 canonical 规则语义，不保存 TOML 路径；
- 三个阶段的 Lua 主程序保存正文、SHA-256 和无损 Windows 解析路径，自动复用正文；
- 术语、Placeholder 与 MV 对话定义继续使用已有权威项目表，不在运行方案中重复存储；
- UI locale、进度模式和日志健康状态不进入项目数据库。

显式 CLI 文件路径以当前工作目录解析。保存的 Lua 路径只提供 chunk 名、`require` 搜索
目录和诊断；脚本主动加载的外部模块、文件或进程仍按执行时环境解析。运行方案的替换
事务与失败语义见 [CLI 现行规格](cli.md#36-成功替换边界) 和 [SQLite 运行时](sqlite.md)。
