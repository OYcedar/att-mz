# RPG Maker Lua 技术参考

本文描述当前 RPG Maker 实现公开给可信 Lua 主程序的运行边界与 `ctx` 接口；当前实现
仅覆盖 MV 与 MZ。
Lua 是 Extract、Translate 和 WriteBack 的显式扩展能力；Init 没有 Lua 扩展面。

这里的“可信”不是沙箱等级。主程序拥有完整 Lua 5.4 标准库，并按 ATT 进程的操作系统
权限运行。`ctx` 提供的是与项目状态、冻结来源、翻译语义和未发布候选相连的受管门面，
不是脚本访问文件、进程或网络的唯一通道。

## 1. 阶段位置与执行顺序

只有命令显式选择 Lua 主程序时，ATT 才读取 `runtime.lua` 并启动 Lua。各阶段的实际位置
如下：

| 命令 | Lua 位置 | 失败与既有结果 |
|---|---|---|
| Init | 不支持 Lua | Init 只建立冻结来源和项目事实 |
| Extract | 所选 Builtin → 所选 Rules → 所选 Lua | 首个失败阻止后续阶段；已经提交的前序 owner 快照不做组合回滚 |
| Translate | Standard → 所选 Lua | Standard 已接受并提交的译文不因后续 Lua 失败而回滚 |
| WriteBack | Standard 重建候选 → 所选 Lua 修改候选 → 全量验证 → 发布 | Lua 失败或取消时丢弃候选；通过验证后才进入唯一发布 |

三个 Lua 入口彼此独立。Extract 使用 Lua 不会自动要求 Translate 或 WriteBack 也使用
Lua；由 `ctx.extract.replace_standard` 建立的标准资产仍可由 Standard Translate 和
Standard WriteBack 消费。

## 2. 运行时与信任边界

ATT 先完整读取主程序，再为当前项目打开一条现存 SQLite 数据库的交互会话。每次调用在
专用 OS 线程中创建、运行并销毁一个 vendored Lua 5.4 VM；SQLite、LLM 和受管文件操作
由 ATT 的异步运行时执行，Lua 线程同步等待结果。

VM 开放 `require`、`io`、`os`、`debug` 等完整标准库。主程序所在目录被加到当前 VM 的
`package.path` 和 `package.cpath`，支持该目录下的 Lua 模块与 Lua 5.4 `luaopen_*` 本机
模块；进程当前工作目录不会因此改变。本机模块句柄保持到 VM 销毁。

因此需要同时理解两种访问方式：

- `ctx.source`、`ctx.output`、`ctx.rpg_maker` 和 `ctx.db` 会执行 ATT 的路径校验、身份
  检查、资源预算和错误分类；
- `io`、`os`、本机模块以及外部进程仍可按操作系统权限直接访问环境，包括
  `ctx.project` 暴露的物理路径。这些操作不会自动获得上述约束，也不会自动参与 ATT 的
  事务、取消或候选发布协议。

Lua 调用位于外层 RPG Maker 项目租约内。脚本同步等待另一个针对同一项目的 ATT 命令，
会与自己持有的租约竞争，并最终得到“项目不存在或正忙”结果。

单次调用的线程栈、VM 内存、指令级取消检查间隔、错误文本上限和 Host 值预算来自
`runtime.lua`。VM 内存上限不约束脚本启动的外部进程，也不为本机模块提供安全隔离。

## 3. `ctx` 总览

下列结构只表示字段和返回形状：

```text
ctx = {
  phase = "extract" | "translate" | "write_back",
  project = {
    name = string,
    source_root = string,
    database_path = string,
    source_language = string,
    target_language = string,
    output_root = string | nil,
  },
  json = JsonApi,
  source = SourceApi,
  rpg_maker = RpgMakerApi,
  db = DatabaseApi,

  extract = ExtractApi | nil,
  translation = TranslationApi | nil,
  llm = function | nil,
  output = OutputApi | nil,
  write_back = WriteBackApi | nil,
}
```

| 阶段 | 存在的阶段接口 | 明确为 `nil` 的阶段接口 |
|---|---|---|
| Extract | `ctx.extract` | `ctx.translation`、`ctx.llm`、`ctx.output`、`ctx.write_back` |
| Translate | `ctx.translation`、`ctx.llm` | `ctx.extract`、`ctx.output`、`ctx.write_back` |
| WriteBack | `ctx.output`、`ctx.write_back` | `ctx.extract`、`ctx.translation`、`ctx.llm` |

`ctx.json`、`ctx.source`、`ctx.rpg_maker` 和 `ctx.db` 在三个阶段始终存在。阶段缺少的接口
不会被替换成一个调用后才报错的占位函数。

项目字段的含义：

- `source_root` 是 Init 冻结内容的物理根。MZ 指向工作区 `source/`，MV 指向工作区
  `source/www/`；它不是原游戏目录。
- `database_path` 是本项目 `project.db` 的物理路径。
- `source_language` 与 `target_language` 来自项目 metadata。
- `output_root` 在 Extract 和 Translate 中为 `nil`；在 WriteBack 中是本次尚未发布的
  暂存候选根路径。它不是最终 `write_back` 发布结果的稳定身份。

这些字段是调用开始时复制到 Lua 的事实快照。修改 Lua table 不会重绑已经建立的来源、
数据库或候选门面。

所有物理路径必须能无损表示为 UTF-8，否则 `ctx` 构造失败。

## 4. 公共接口

### 4.1 无损 JSON：`ctx.json`

```text
ctx.json.NULL
ctx.json.array(table | nil) -> table
ctx.json.object(table | nil) -> table
ctx.json.number(text) -> integer | JsonNumber
ctx.json.decode(text) -> JsonValue
ctx.json.encode(value) -> string
ctx.json.kind(value) -> "null" | "boolean" | "number" | "string" | "array" | "object" | nil
ctx.json.number_text(number) -> string
```

该接口无损区分 JSON null、array、object、string、boolean 与任意精度 number：

- JSON null 使用 `ctx.json.NULL`，不是 Lua `nil`；
- JSON array 与 object 都是 Lua table，但必须带有各自的私有类型标记；
- `array` 和 `object` 只标记既有 table，传入 `nil` 时创建空 table，不在标记时遍历内容；
- 只有文本本身就是标准 i64 十进制形式的数字才成为 Lua integer；`-0`、`1e0` 等虽能
  表示相同数值但文本形式不同的数字仍使用 `JsonNumber` userdata。`number_text` 可取回
  精确数字文本；
- `decode` 必须完整消费一份 UTF-8 JSON，并拒绝重复 object key；`encode` 拒绝未标记
  table、数组洞、非字符串 object 键、循环引用、NaN、Infinity 和非 JSON userdata；
  输出使用紧凑确定性格式，按 object key 排序，并保留 `JsonNumber` 的精确数字文本。

`ctx.json` 自身的参数、结构、语法、编码和预算错误统一为
`domain="json"`、`kind="invalid_value"`。这条规则只适用于 `ctx.json`；
`ctx.source.read_json`、`ctx.output.read_json` 和 `ctx.output.write_json` 的转换失败位于各自
绑定边界，错误域不同。

### 4.2 冻结来源：`ctx.source`

```text
ctx.source.read(path) -> string          -- 原始字节
ctx.source.read_text(path) -> string     -- UTF-8
ctx.source.read_json(path) -> JsonValue
ctx.source.list(path) -> string[]
```

来源路径使用正斜杠形式的逻辑相对路径，必须从 `data` 或 `js` 开始，不得为空、绝对、
包含当前段、父级逃逸、NTFS ADS 或非 UTF-8 路径段。MV/MZ 都使用相同的逻辑路径；MV 的
物理 `www` 已包含在 `source_root` 中。

`list` 只列出指定目录的直接子项，返回经过稳定排序的完整逻辑子路径。该门面只读 Init
冻结副本，不重新访问外部游戏根。文件不存在、目标种类错误和底层 I/O 使用
`filesystem` 错误域；无效来源路径使用 `binding/invalid_source_path`。
`read_text` 的 UTF-8 失败和 `read_json` 的 UTF-8、JSON 或 Lua 值转换失败使用
`binding/invalid_value`。

### 4.3 RPG Maker 来源与位置：`ctx.rpg_maker`

```text
ctx.rpg_maker.DECODE_JSON
ctx.rpg_maker.data(file_name) -> RpgMakerSource
ctx.rpg_maker.map(map_id) -> RpgMakerSource
ctx.rpg_maker.plugin_parameter(plugin_index, plugin_name, parameter_name) -> RpgMakerSource
ctx.rpg_maker.open(source) -> RpgMakerDocument

document:value(path) -> JsonValue
document:location(path) -> RpgMakerLocation
document:text(path) -> RpgMakerTextRef
document:note_tag(container_path, tag_name, occurrence) -> RpgMakerTextRef
document:comment_tag(command_path, tag_name, occurrence) -> RpgMakerTextRef

text_ref.original -> string
text_ref.location -> RpgMakerLocation
```

`data` 只接受当前标准数据文件名：`Actors.json`、`Animations.json`、`Armors.json`、
`Classes.json`、`CommonEvents.json`、`Enemies.json`、`Items.json`、`MapInfos.json`、
`Skills.json`、`States.json`、`System.json`、`Tilesets.json`、`Troops.json` 和
`Weapons.json`。地图使用正 u32 ID。插件参数的 `plugin_index` 是 `js/plugins.js`
数组的零基下标；索引、插件名和参数名会在 `open` 时重新核对，避免位置漂移后读取另一
项。

RPG Maker path 是一个无洞 Lua 数组。每一步可以是：

- UTF-8 字符串：JSON object 键；
- 非负 Lua integer：JSON array 的零基下标；
- `ctx.rpg_maker.DECODE_JSON`：把当前 JSON string 再解码一层后继续定位。

`document:value` 返回无损 JSON 值；`location` 在确认路径存在后返回受信位置；`text`
进一步要求终值是 string，并返回同时携带冻结原文与位置的不可伪造引用。位置对象的
字符串表示只用于诊断，不应反向解析成协议身份。

`ctx.rpg_maker.open` 始终打开冻结来源，底层文件读取失败仍使用 `filesystem` 错误域。
它不会打开 WriteBack 候选；候选 JSON 应通过 `ctx.output.read_json` 读取。

`note_tag` 的路径指向含字符串 `note` 的 object；`comment_tag` 的路径必须终止于事件
命令数组中一条 `108` 的零基下标，并把紧随的 `408` 行合并后定位标签。标签
`occurrence` 为零基序号，标签名不能为空且不能包含 `<`、`>` 或 `:`。这两个接口使用
共享标签语义，只返回标签值对应的受信文本引用，不按键名递归猜测结构。

RPG Maker 参数错误使用 `rpg_maker/invalid_argument`。文档错误使用 `rpg_maker`
错误域，当前公开 kind 包括 `invalid_source`、`invalid_plugin_parameter_source`、
`resource_limit`、`invalid_utf8`、`invalid_json`、`invalid_plugins_envelope`、
`invalid_location`、`invalid_tag` 和 `tag_not_found`。其中整份文档超过 Host 最大字节数
明确是 `rpg_maker/resource_limit`，不是通用 Host 值预算错误。

### 4.4 SQLite：`ctx.db`

```text
ctx.db.NULL
ctx.db.blob(bytes) -> Blob
blob:bytes() -> string
ctx.db.query(sql, parameters | nil) -> rows
ctx.db.execute(sql, parameters | nil) -> changed_rows
ctx.db.begin() -> nil
ctx.db.commit() -> nil
ctx.db.rollback() -> nil
```

SQL 参数必须是 `nil` 或从 1 开始的无洞数组。支持的值为 Lua integer、有限 Lua
number、UTF-8 Lua string、`ctx.db.NULL` 和 `ctx.db.blob`；普通 Lua string 始终按
SQLite TEXT 绑定，不能用来隐式表示 BLOB。SQLite 参数不支持 boolean；`ctx.db.NULL`
与 `ctx.json.NULL` 是两种不同的 userdata，不能互换。

`query` 按查询列顺序返回二维无洞数组，不返回列名。结果 NULL、INTEGER、有限 REAL、
TEXT 和 BLOB 分别成为 `ctx.db.NULL`、Lua integer、Lua number、Lua string 和 Blob
userdata。`execute` 返回受影响行数。

`begin` 固定建立 `BEGIN DEFERRED` 事务。正常返回时若仍有活动事务，唯一终结器会尝试
回滚，并把整次 Lua 调用报告为未关闭事务错误；这不是一次隐式提交。显式提交和事务外
自动提交已经产生的副作用不会因为脚本之后失败或取消而自动撤销。SQLite
`outcome_unknown` 后会话进入不可继续状态，后续操作返回 `indeterminate`，最终仍由
终结器尝试观察与收尾。

## 5. Extract 接口

```text
ctx.extract.replace_standard(groups) -> nil
ctx.extract.clear_standard() -> nil

groups = Group[]
Group = {
  kind = "database_entry" | "system" | "map" | "event_command" | "plugin_parameter",
  location = RpgMakerLocation,
  fields = Field[],
}
Field = {
  name = non_empty_string,
  text = RpgMakerTextRef,
}
```

Group 与 Field 只允许上述精确字段。`location` 必须由 `document:location` 建立且是普通
Value 位置；`text` 必须由已打开文档建立。每个组至少有一个字段，字段原文不能全为空
白，同一逻辑身份或物理修改目标不能重复。组类型、来源、位置种类还必须满足标准资产
矩阵，例如 `system` 只接受 `System.json`，`map` 只接受地图来源，
`plugin_parameter` 只接受插件参数来源；Note Tag 只适用于数据库、System 和 Map 类，
Comment Tag 只适用于 `event_command`。

该接口只建立“一个标量语义字段 → 一个受信物理文本位置”的直接配方，不提供对话行、
选择行、滚动文本行或一对多投影的构造入口。

一次 Extract 主程序最多声明一个意图：调用一次 `replace_standard` 或一次
`clear_standard`。第二次声明返回 `extract/intent_already_declared`。
`replace_standard({})` 表示 Lua owner 处于 active 状态但当前快照为空；
`clear_standard()` 表示停用 Lua owner 并删除其标准资产。没有调用任何一个接口表示
保持现有 Lua owner 不变。

快照的形状、来源、位置或标准资产模型不合法时返回
`extract/invalid_standard_snapshot`；Host 值超预算仍使用通用的
`binding/host_value_budget_exceeded`。

这两个函数在 VM 内只记录经过完整校验的内存意图，不直接写标准资产表。只有同时满足
以下条件，Extract 服务才会在 Host 结束后用一个独立事务应用意图：

- 主程序干净返回；
- 没有取消；
- SQLite 会话终结成功；
- 不存在未关闭事务。

脚本失败、取消、清理失败或未关闭事务时，意图不会交给标准资产 Store。Store 的独立
事务仍可能因 owner 冲突、来源变化、数据库失败或提交结果不确定而失败。

脚本通过 `ctx.db` 执行的 SQL 与该意图不在同一事务中。已经自动提交或显式提交的 SQL
不会因意图未应用而回滚；反过来，意图 Store 失败也不会撤销这些 SQL。自建表与标准
资产意图之间若存在交接关系，幂等和恢复语义必须由脚本的数据协议明确建立。

## 6. Translate 接口

Standard Translate 先完成本轮标准资产处理，并把本轮已经解析的 Prompt、语言对、术语、
占位符和源语言分析语义交给 Lua。Lua 不重新读取这些资源。

```text
ctx.translation.system_prompt -> string
ctx.translation.language_pair -> { source = string, target = string }
ctx.translation.prepare(kind, original) -> PreparedText

kind = "database_entry" | "system" | "map" | "dialogue" |
       "choices" | "scrolling_text" | "event_command" | "plugin_parameter"

PreparedText = {
  status = "active" | "non_source_language" | "fully_protected",
  model_text = string,
  terms = { { term = string, translation = string }, ... },
  accept = function,
}

PreparedText:accept(candidate) ->
  { accepted = true, translation = string }
  | { accepted = false, reason = string }
```

`prepare` 只处理一个原始字符串。`model_text` 是占位符保护后的模型输入；`terms` 是这段
原文实际触发的有序术语。只有 `active` 表示需要模型翻译；另外两个状态分别表示源语言
分析认为无需翻译，或去除保护内容后没有可翻译的自然语言。

`accept` 复用 Standard 的候选验收、占位符恢复、源语残留检查和可选修复。正常的内容
拒绝不是 Host 错误，而是 `accepted=false`；当前 reason 包括
`non_source_language`、`fully_protected`、`missing`、`duplicate`、`invalid_shape`、
`line_count_mismatch`、`invalid_line_text`、`blank_line_mismatch`、
`blank_translation`、`no_natural_language_text`、`contains_byte_order_mark`、
`placeholder_mismatch`、`unexpected_placeholder_token`、
`placeholder_normalization_ambiguous` 和 `source_residual`。

`prepare` 与 `accept` 都不读写译文表，不建立 `Current` 状态，也不证明 Lua 自有数据已经
持久化。成功验收只返回一个恢复后的单字符串译文；如何批次化、怎样与自有身份关联、
何时写入 `ctx.db` 以及事务多大，属于脚本自己的数据协议。

### 6.1 LLM 调用

```text
ctx.llm(messages) -> {
  content = string,
  finish_reason = string,
  request_id = string | nil,
  response_id = string | nil,
  usage = {
    prompt_tokens = integer,
    completion_tokens = integer,
    total_tokens = integer,
  } | nil,
}

messages = {
  { role = "system" | "user" | "assistant", content = string },
  ...
}
```

`messages` 必须是无洞数组，每项必须恰好包含 `role` 和 `content`。Lua 使用当前 Profile
已经选择的公共 Client 和非流式 Chat Completions 执行器，不能在调用中覆盖 URL、API
key、model、parameters 或 stream。`ctx.translation.system_prompt` 只是本轮解析后的
Prompt 文本，`ctx.llm` 不会把它自动插入 `messages`。

`ctx.llm` 成功返回供应商响应即产生上述结果；`finish_reason` 不是 `stop` 也不会自动变成
Host 错误。是否接受、继续或重试由脚本依据 `finish_reason` 和 `content` 判断。

LLM 根不自动重试。暂时失败抛出 `llm/retryable`，并可能在
`retry_after_ms` 给出建议等待时间；不可恢复失败为 `llm/fatal`。是否重试、怎样限量和
何时停止由脚本决定。请求即使最终报错或调用随后取消，也可能已被外部服务接收，不能把
“没有返回响应”解释成“没有外部副作用”。

## 7. WriteBack 接口

### 7.1 受管候选：`ctx.output`

```text
ctx.output.read(path) -> string
ctx.output.read_text(path) -> string
ctx.output.read_json(path) -> JsonValue
ctx.output.list(path) -> { { name = string, kind = "file" | "directory" }, ... }
ctx.output.create_directory(path) -> nil
ctx.output.write(path, bytes) -> nil
ctx.output.write_text(path, text) -> nil
ctx.output.write_json(path, value) -> nil
ctx.output.remove(path) -> nil
```

这些路径是 RPG Maker 内容根内的安全相对路径。通常从 `data` 或 `js` 开始：MZ 直接映射
到候选的 `data/`、`js/`，MV 则在 Host 边界映射到候选的 `www/data/`、`www/js/`。
路径必须包含至少一个普通段，拒绝绝对路径、当前段、父级逃逸和 ADS。MZ 的绑定范围只
允许 `data` 与 `js`；MV 的绑定范围是 `www`，但最终结构验证仍只接受其中恰好存在
`data` 与 `js` 两个普通目录。

`list` 以稳定顺序返回直接子项的名称和种类。`remove` 不递归删除非空目录，此时返回
`output/directory_not_empty`。`write_json` 使用与 `ctx.json.encode` 相同的无损编码
模型，但其编码失败通过输出绑定报告为 `binding/invalid_value`。候选编辑错误使用
`output` 错误域；安全相对路径自身无效使用 `binding/invalid_output_path`。

`ctx.output` 绑定到当前暂存候选的物理身份，并执行候选资源限制与操作终态约束。它是
受管编辑入口，不是文件系统沙箱：脚本仍能通过标准库直接使用
`ctx.project.output_root`。直接 I/O 会绕过 `ctx.output` 的路径、预算和身份检查，但只要
改动落在候选树中，仍要面对后续全量候选验证。

### 7.2 保守布局：`ctx.write_back.layout`

```text
ctx.write_back.layout(region, pairs) -> {
  status = "applied" | "manual",
  texts = string[],
  inserted_line_breaks = integer,
  inserted_fullwidth_indents = integer,
}

region = "dialogue_body" | "scrolling_text" | "help_description"
pairs = {
  { original = string, translation = string | nil },
  ...
}
```

每个 pair 只能有 `original` 和 `translation` 两个字段。`translation=nil` 表示该项必须
保留冻结原文：它参与跨项括号和缩进状态观察，但布局器不得修改它。`texts` 与输入逐项
对齐。

`applied` 表示共享布局器能够安全新增换行或全角续行缩进；两个计数只统计本次新增内容。
`manual` 是正常业务结果，表示控制符、断点或阅读质量无法安全自动处理。此时 `texts`
仍与输入逐项对齐且不含程序新增内容，两个计数为零。返回值没有 `reason` 字段。

Lua 结束后，WriteBack 会对完整候选重新执行目录身份与资源验证，并检查：MZ 顶层恰好
是普通 `data`、`js` 目录；MV 顶层恰好是普通 `www` 目录，且其下恰好是普通
`data`、`js` 目录。验证成功后 Publisher 才按值接管候选并发布；Lua 没有受管的
validate、discard 或 publish 接口。

## 8. Host 值预算与错误边界

`runtime.lua.host_values` 的 `max_bytes`、`max_nodes` 和 `max_depth` 约束 Lua 与 Host
之间的动态值。根值深度为 1，容器和标量都计节点；动态 UTF-8 字符串与二进制按原始
字节计入。固定协议字段名不重复计入调用值。标记或查询 JSON table 类型不遍历内容，
真正编码或跨 Host 边界时才计算整棵值。

同一数值预算会按所属接口映射为不同错误，不能只判断一个统一 kind：

| 场景 | 公开错误 |
|---|---|
| `ctx.json` 的解析、编码或 JSON 预算失败 | `json/invalid_value` |
| 一般参数、返回值、SQL、LLM、布局或标准快照跨界超预算 | `binding/host_value_budget_exceeded` |
| `ctx.rpg_maker.open` 的整份文档超过最大字节数 | `rpg_maker/resource_limit` |
| `source/output` 的文本或 JSON 转换失败 | `binding/invalid_value` |

Host 错误作为 userdata 抛出，可由 `pcall` 读取：

```text
error.domain = string
error.kind = string
error.message = string
error.retry_after_ms = integer | nil
```

`domain` 与 `kind` 用于分支；`message` 只用于诊断，不是可解析协议。未捕获的 Host 错误
保留其身份并成为 Lua Binding 失败；普通 Lua 错误、编译错误、上下文错误、取消、worker
panic 和清理错误保持不同终态。

除前文已经列出的错误外，常见稳定错误包括：

- SQLite：`sqlite/closed`、`indeterminate`、`transaction_already_active`、
  `no_active_transaction`、`operation_failed`、`outcome_unknown`；
- 来源文件：`filesystem/not_found`、`not_file`、`not_directory`、`invalid_path`、
  `invalid_utf8`、`io`；
- 候选编辑：`output/outside_scope`、`scope_root_mutation`、`not_found`、`not_file`、
  `not_directory`、`directory_not_empty`、`candidate_identity_changed`、
  `wrong_editor_instance`、`invalid_utf8_name`、`io`；
- Translate 语义技术失败：`translation/prepare` 或 `translation/accept`；
- Host 桥：`runtime/cancelled` 或 `runtime/host_bridge_closed`。

## 9. 取消、终结与副作用

取消是合作式的：VM 按配置的指令间隔检查取消；等待普通来源、SQLite 或 LLM Host 调用
时，ATT 可终止等待桥；候选目录操作一旦交给文件系统根，则必须先等待该次操作到达明确
终态，再向 Lua 返回 `runtime/cancelled`。主程序即使捕获取消错误，VM 返回边界仍会再次
观察取消状态。

完整标准库会限制这种保证。脚本可以替换 debug hook、长时间停留在 `os.execute` 或
本机模块中，也可能通过 `os.exit` 或本机崩溃直接终止进程；这类情况没有强制抢占、超时
卸载或进程存活保证。

Runtime 一旦同步接管 bindings，无论线程创建失败、VM 失败、取消或 worker panic，监督
者都会保留唯一 SQLite 终结权并尝试恰好终结一次。执行终态与清理终态分别保留；清理
失败不会覆盖先发生的执行错误。丢弃执行句柄只请求取消，监督者仍继续完成线程回收与
终结。Runtime shutdown 会停止接收新调用、请求取消全部已经接管的 job，并等待它们的
worker 与唯一终结器结束；只有执行终态为取消且终结器成功时，外层才把它作为正常的
`Cancelled` 完成，清理失败仍是错误。

需要按副作用类型判断失败后的现实状态：

- 活动 SQLite 事务由终结器尝试回滚；已提交和自动提交内容保持；
- Extract 的内存标准快照意图只有干净终结后才可能进入独立 Store 事务；
- Translate 的 Standard 结果、Lua 已提交 SQL 和已经发送的 LLM 请求不做组合回滚；
- WriteBack 的 Lua 修改只位于暂存候选，Lua 失败或取消时外层尝试丢弃该候选；
- 标准库、本机模块和外部进程产生的其他副作用不属于 ATT 的自动恢复边界。

相关配置见[运行配置](../runtime/configuration.md)，标准资产与阶段行为分别见
[提取](extraction.md)、[翻译](translation.md)和[写回](write-back.md)。
