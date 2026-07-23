# RPG Maker Lua 技术参考

本文定义 MV/MZ 向可信 Lua 5.4 主程序公开的当前接口。Lua 用于 Extract、Translate、
WriteBack 的阶段扩展；Init 没有 Lua。可复制的完整协议见 [Lua Cookbook](lua-cookbook.md)
和 [`examples/`](examples/README.md)。

“可信”不是沙箱：脚本拥有完整 Lua 标准库，并按 ATT 进程的操作系统权限运行。`ctx`
提供的是与冻结来源、翻译语义、SQLite 和未发布候选相连的受管门面，不是唯一访问路径。

本文所有代码块前使用 `<!-- att-example: valid|invalid|illustrative -->`：valid 是当前 API
可用代码，invalid 是必须失败的反例，illustrative 只展示形状或数据。

## 1. 三阶段位置与停止线

| 命令 | Lua 位置 | 失败边界 |
|---|---|---|
| Init | 不支持 | 只建立项目与冻结来源 |
| Extract | Builtin → Rules → Lua | 已提交前序 owner 不组合回滚；Lua 意图仅在脚本干净终结后提交 |
| Translate | Standard → Lua | Standard 已提交译文不因 Lua 失败回滚 |
| WriteBack | Standard 候选 → Lua → 验证 → 发布 | Lua 失败丢弃候选；没有发布后回调 |

三个入口独立。简单标量可在 Extract 用 `replace_standard` 接入 Standard Translate 和
WriteBack；复杂跨文档/多目标插件由 Lua 自己拥有三阶段身份、私有表、state、事务和幂等
写回。核心不增加通用多目标投影 DSL、自动发布状态或 post-publish hook。

每个阶段独立保存自己的 Lua 主程序快照：非空主程序正文、SHA-256 和无损 Windows 解析
路径。显式提供非空 `--lua` 时精确替换该阶段快照；省略 `--lua` 时，仅在上次成功运行
方案启用了 Lua 的情况下复用；零字节文件显式清除该阶段主程序。自动复用执行数据库中的
正文，不重新读取原文件；保存路径只用于 chunk 名、`require` 搜索目录和诊断。主程序通过
`require`、`io`、`os` 或本机模块动态读取的模块、文件和进程仍是外部依赖，不纳入快照。

清除语义按阶段区分：Extract 同时停用 Lua owner、删除其标准资产，并从后续自动方案中
移除 Lua；Translate 与 WriteBack 只清除各自主程序，不猜测或删除 Lua 私有数据库状态。
三个阶段即使最初来自同一个文件，也仍是彼此独立的快照和运行方案。

## 2. VM、连接与 `ctx`

每次阶段调用都创建新的 OS worker、Lua VM 和 SQLite 连接，主程序结束后销毁。因此：

- Lua globals、`package.loaded`、闭包、userdata 不跨阶段；
- SQLite TEMP 表、临时 pragma 和连接状态不跨阶段；
- 只有持久数据库表、冻结来源、标准资产或已发布文件能跨阶段交接；
- Extract、Translate、WriteBack 即使保存了同一路径，也不能依赖前一 VM 的内存。

主程序目录加入当前 VM 的 `package.path`/`package.cpath`；进程 cwd 不改变。`require`、
`io`、`os`、`debug` 与 Lua 5.4 本机模块开放。直接 I/O/进程/本机模块不自动进入 ATT 的
受管路径、取消、事务或候选发布协议。

<!-- att-example: illustrative -->
```lua
ctx = {
  phase = "extract" | "translate" | "write_back",
  project = {
    name = string,
    engine = "mv" | "mz",
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

| 阶段 | 非 nil 的阶段接口 |
|---|---|
| Extract | `ctx.extract` |
| Translate | `ctx.translation`、`ctx.llm` |
| WriteBack | `ctx.output`、`ctx.write_back` |

`json/source/rpg_maker/db` 三阶段都有。`source_root` 是冻结内容物理根：MZ 对应 `source/`，
MV 对应 `source/www/`。`output_root` 仅 WriteBack 存在且是未发布候选物理路径。这些物理
路径只供可信脚本诊断或显式直接 I/O；受管 API 一律使用下一节逻辑路径。

## 3. 跨平台逻辑路径

`ctx.source` 与 `ctx.output` 使用相同逻辑空间，MV/MZ 都只看 `data/...`、`js/...`；MV 的
`www` 只由 Host 内部映射，脚本不能写 `www/...`。

<!-- att-example: illustrative -->
```ebnf
logical-path = root, { "/", segment } ;
root         = "data" | "js" ;
segment      = one or more UTF-8 characters other than "/", "\\", ":" or control ;
```

根必须小写。拒绝空字符串、绝对路径、反斜杠、空段、`.`、`..`、重复或尾随 `/`、冒号、
控制字符和非 UTF-8。Host 从 `data`/`js` 根开始逐段解析，每个中间目录和最终文件/目录
都必须与实际目录项逐字同大小写；请求 `data/items.json` 而实际为 `data/Items.json` 时
抛出 `filesystem/case_mismatch`，不借助 Windows 大小写别名。真实不存在仍按具体操作
返回既有 `filesystem/not_found`。

<!-- att-example: valid -->
```lua
local actors = ctx.source.read_json("data/Actors.json")
local plugins = ctx.source.read_text("js/plugins.js")
```

<!-- att-example: invalid -->
```lua
ctx.source.read([[data\Actors.json]])
```

## 4. 公共 JSON 与冻结来源

### 4.1 `ctx.json`

<!-- att-example: illustrative -->
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

JSON null 是 `ctx.json.NULL`，不是 nil。array/object 都是带私有类型标记的 Lua table；
未标记 table 不能编码。解码拒绝重复 object key；编码拒绝数组洞、非字符串 object key、
循环、NaN/Infinity 和非 JSON userdata。紧凑输出按 object key 字节顺序排序，并保留无损
number 文本；`JsonNumber` 用 `number_text` 取回原始文本。

### 4.2 `ctx.source`

<!-- att-example: illustrative -->
```text
ctx.source.read(path) -> string
ctx.source.read_text(path) -> string
ctx.source.read_json(path) -> JsonValue
ctx.source.list(path) -> string[]
```

`read` 返回字节；`read_text` 要求 UTF-8；`read_json` 使用无损 JSON；`list` 只列直接子项，
返回稳定排序的完整逻辑子路径。门面只读 Init 冻结副本，不访问原游戏。

## 5. `ctx.rpg_maker` 来源、路径与标签

<!-- att-example: illustrative -->
```text
ctx.rpg_maker.DECODE_JSON
ctx.rpg_maker.data(file_name) -> RpgMakerSource
ctx.rpg_maker.data_file(file_name) -> RpgMakerSource
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

`data` 只接受以下标准数据文件精确名：`Actors.json`、`Animations.json`、`Armors.json`、
`Classes.json`、`CommonEvents.json`、`Enemies.json`、`Items.json`、`MapInfos.json`、
`Skills.json`、`States.json`、`System.json`、`Tilesets.json`、`Troops.json`、
`Weapons.json`。`data_file` 接受
[规则文件定义的安全精确 JSON 基名](rules.md#48-文件身份map-和实际大小写)，并自动收敛身份：

- 标准文件复用 `data(...)` 的标准身份；
- 规范 `Map001.json`、`Map1000.json` 复用正 `MapId`；
- `Map000.json`、`Map01.json` 等安全近似名是自定义 DataFile；
- 实际目录项大小写不一致在 `open` 时明确失败。

`map` 只接受 1～`u32::MAX` 的 Lua integer；不存在 Map 0。`plugin_index` 是
`js/plugins.js` 零基下标。显式 Lua source 可以读取 `status=false` 的插件参数；Host 仍
核对 index、name、parameter，避免位置漂移。Rules 只扫描启用插件，两者不要混淆。

<!-- att-example: valid -->
```lua
local custom = ctx.rpg_maker.data_file("QuestEntries.json")
local map = ctx.rpg_maker.map(1)
local document = ctx.rpg_maker.open(custom)
```

RPG Maker path 是从 1 开始、无洞的 Lua 数组，但其中 JSON array index 是**零基**整数：

- string：object key，可为空；
- 非负 integer：array 零基下标；
- `DECODE_JSON`：当前值必须是 string，解码一层后继续。

<!-- att-example: valid -->
```lua
local path = ctx.json.array({ 1, "payload", ctx.rpg_maker.DECODE_JSON, "title" })
local text = document:text(path)
```

`document:value` 每次返回与冻结文档脱离的深拷贝；修改返回 table 不会修改文档、已有引用
或随后读取。`location`/`text` 由 Host 建立不可伪造身份，显示字符串只用于诊断。

NoteTag 与 CommentTag 只识别简单 `<name:value>`：第一个冒号分隔名和值，第一个后续 `>`
结束；没有冒号、空名或缺少 `>` 不是标签。occurrence 按同名标签分别从 0 计数。tag name
参数不能为空，不能含 `<`、`>`、`:`。

`note_tag` 的 container 指向含 string `note` 的 object。`comment_tag` 的 command path
必须指向一条 code 108；Host 将它与紧随的连续 code 408 的 `parameters[0]` 用 LF 拼接，
再定位标签。WriteBack 使用同一 108+408 recipe 和 occurrence。

两种路径都可以包含一个或多个 `DECODE_JSON`。WriteBack 会沿相同路径逐层解码，完成全部
类型、occurrence 和冻结原文验收后，再从内向外编码为紧凑 JSON string；任一层失败都不会
提交该文档候选。被解码层原有的 JSON 排版空白不会保留，未修改的 JSON 值与 UTF-8 文本保留。

<!-- att-example: valid -->
```lua
local entry = ctx.json.array({ 1 })
local help = document:note_tag(entry, "Help", 0)

local command = ctx.json.array({ "events", 1, "pages", 0, "list", 12 })
local quest = document:comment_tag(command, "Quest", 0)
```

## 6. Extract：`replace_standard`

<!-- att-example: illustrative -->
```lua
ctx.extract.replace_standard(groups)
ctx.extract.clear_standard()

groups = {
  {
    kind = "database_entry" | "system" | "map" | "event_command" | "plugin_parameter",
    location = RpgMakerLocation,
    fields = { { name = non_empty_string, text = RpgMakerTextRef }, ... },
  },
  ...
}
```

完整 source × kind × exact location 矩阵：

| kind | 合法 source | field `text` 位置 |
|---|---|---|
| `database_entry` | 除 System 外的标准 Data、自定义 DataFile、Map | Value 或 NoteTag |
| `system` | 仅标准 `System.json` | Value 或 NoteTag |
| `map` | 仅规范 Map | Value 或 NoteTag |
| `event_command` | 标准 Data、自定义 DataFile、Map | Value 或 CommentTag |
| `plugin_parameter` | 显式 PluginParameter source | Value |

`location` 必须是同一 source 的普通 Value；NoteTag container 或 CommentTag command path
必须等于组 location。Lua 不通过此接口构造 Dialogue、Choices、ScrollingText 或一对多
投影。

`groups` 和 `fields` 都是从 1 开始无洞数组；声明顺序分别成为 `group_order` 和
`unit_order`，不得按字母重排。同一次调用中 `group.location` 本身必须唯一；同位置同 kind
或不同 kind 都是 `extract / invalid_standard_snapshot`，Host 不会把它们自动合并。每组至少
一个 field；name 非空且组内唯一；原文不能全空白；逻辑身份和 Mutation Claim 不得冲突。

一次脚本最多声明一个意图：一次 `replace_standard` 或 `clear_standard`。第二次失败。
`replace_standard({})` 是 active 的空 Lua owner；`clear_standard()` 停用 owner；不调用
表示保持旧 owner。

意图先完整校验并留在内存。只有脚本正常返回、未取消、SQLite 连接干净终结且无活动事务
时，Host 才用独立 Store 事务应用。脚本通过 `ctx.db` 的提交不和 Store 事务合并。

## 7. Translate：prepare、Current 与 accept

<!-- att-example: illustrative -->
```lua
ctx.translation.system_prompt -> string
ctx.translation.language_pair -> { source = string, target = string }

prepared = ctx.translation.prepare(kind, original, semantic_context)

prepared.status -> "active" | "non_source_language" | "fully_protected"
prepared.model_text -> string
prepared.terms -> { { term = string, translation = string }, ... }

prepared:is_current(translation, state) -> boolean

prepared:accept(candidate) ->
  { accepted = true, translation = string, state = string }
  | { accepted = false, reason = string }
```

kind 精确值为 `database_entry`、`system`、`map`、`dialogue`、`choices`、
`scrolling_text`、`event_command`、`plugin_parameter`。`original` 是一个标量 string。
`semantic_context` **必填**；无脚本私有语义时传 `""`。自定义 Prompt、Speaker、邻文、
外部模型版本或其他 Host 不掌握、但会改变正确译文的事实，必须由脚本稳定编码进去。

`model_text` 是 Placeholder 后输入；`terms` 是仅对 NaturalText 的有序命中，和 Standard
state/Prompt 共用结果。`prepare` 不读写脚本私有译文。

state 是不透明的 64 字符小写十六进制 SHA-256 文本，自动绑定 engine、语言对、语言
模块、公共 Prompt/Client 语义、kind、original、实际 Placeholder、实际有序术语、脚本
context 和最终译文；规则编号、未命中资源、并发、重试不进入。

`is_current` 只用当前 prepared 语义和传入 translation 重算 state 后比较，不再次验收或
正规化旧译文。state 不是 string、长度不是 64、含大写或非十六进制字符时抛出
`translation/invalid_state`，不返回 false。

<!-- att-example: valid -->
```lua
local prepared = ctx.translation.prepare("plugin_parameter", original, "protocol=quest-title")
if saved_translation ~= nil and saved_state ~= nil
   and prepared:is_current(saved_translation, saved_state) then
  return saved_translation
end
```

`accept` 是标量验收：允许并原样保留 LF；拒绝 CR、NUL 和全空白。它继续检查 BOM、自然
语言、Placeholder、源语残留及语言修复。当前可达 reason 只有：

| reason | 含义 |
|---|---|
| `non_source_language` | prepared 本就无需翻译 |
| `fully_protected` | prepared 没有 NaturalText |
| `contains_carriage_return` | candidate 含 CR |
| `contains_nul` | candidate 含 NUL |
| `blank_translation` | candidate 全为空白 |
| `no_natural_language_text` | 恢复后没有目标自然语言 |
| `contains_byte_order_mark` | 含 BOM |
| `placeholder_mismatch` | 预期 token 数量/槽不符 |
| `unexpected_placeholder_token` | 出现未声明 ATT token |
| `placeholder_normalization_ambiguous` | 重复槽无法无歧义恢复 |
| `source_residual` | 语言分析/修复后仍残留不可接受源语 |

Standard 多 ID/Lines 响应的结构错误不属于 Lua 标量 accept 的 reason 集合。

成功 acceptance 的 translation/state 必须由脚本在同一 SQLite 事务中成对写入私有表。
核心不替 Lua 选择身份或事务粒度。

`non_source_language` 与 `fully_protected` 没有可接受的 candidate；推荐成对删除旧
translation/state 并保持 NULL。以后再次运行 Translate 会重新 `prepare`，但不会请求
LLM。Lua 私有 Current 也只在 Translate 脚本实际调用 `is_current` 时检查：任何已绑定语义
变化后，都必须先重新运行同一 Translate 脚本，再进入 WriteBack；核心不会替私有协议
读取或刷新其表。

## 8. `ctx.llm`

<!-- att-example: illustrative -->
```lua
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
```

messages 是无洞数组，每项恰有 `role = system|user|assistant` 和 `content`。调用复用当前
Profile 的 Client，但不会自动插入 system prompt；脚本应显式发送
`ctx.translation.system_prompt`。不能覆盖 URL、API key、model、parameters 或 stream。

返回供应商响应即成功；`finish_reason ~= "stop"` 不自动报错。LLM 根不自动重试；
`llm/retryable` 可带 `retry_after_ms`，`llm/fatal` 不可恢复。请求失败也可能已被服务接收。

## 9. SQLite：私有协议与事务

<!-- att-example: illustrative -->
```text
ctx.db.NULL
ctx.db.blob(bytes) -> Blob
blob:bytes() -> string
ctx.db.query(sql, parameters | nil) -> rows
ctx.db.execute(sql, parameters | nil) -> changed_rows
ctx.db.begin()
ctx.db.commit()
ctx.db.rollback()
```

`query`/`execute` 各只接受**一条**完整 SQL statement；可信脚本保留 SQLite 对该单条语句
允许的完整能力。参数是 nil 或从 1 开始无洞数组，值可为 integer、有限 number、UTF-8
TEXT、`ctx.db.NULL`、`ctx.db.blob`；boolean 不支持。结果按列顺序返回二维数组，无列名。

`begin` 固定 `BEGIN DEFERRED`。正常返回仍有事务时，终结器尝试回滚，并把调用报告为
未关闭事务错误；不隐式 commit。已自动提交或显式提交的内容不因脚本后来失败而回滚。
`outcome_unknown` 后连接不可继续。

官方支持的扩展路径只保证脚本自己的私有命名空间表，例如 `lua_quest_translation`。
脚本可以直接读写 ATT 受管表，但这属于高级操作：作者自行承担全部 schema、身份、顺序、
完整逻辑 Claim、冲突摘要、指纹、译文/state 和事务不变量。位置、Mutation resource、
unit role 与 recipe 必须写成当前 compact canonical JSON；`standard_mutation_claim`
只保存从 recipe 派生的确定性跨 owner 冲突摘要，不是完整 Claim 清单。ATT 不承诺受管
schema 是稳定扩展 API。不要在官方范例中这样做。

因为每阶段是新连接，TEMP 表和未持久化连接状态不能作为跨阶段协议。持久私有表是推荐
交接位置。

## 10. WriteBack 候选与布局

### 10.1 `ctx.output`

<!-- att-example: illustrative -->
```text
ctx.output.read(path) -> string
ctx.output.read_text(path) -> string
ctx.output.read_json(path) -> JsonValue
ctx.output.list(path) -> { { name = string, kind = "file" | "directory" }, ... }
ctx.output.create_directory(path)
ctx.output.write(path, bytes)
ctx.output.write_text(path, text)
ctx.output.write_json(path, value)
ctx.output.remove(path)
```

所有路径使用第 3 节严格逻辑路径；MV/MZ 规则相同。操作矩阵：

| 操作 | 文件 | 非根目录 | 根 `data`/`js` |
|---|:---:|:---:|:---:|
| `read/read_text/read_json` | 是 | `not_file` | `not_file` |
| `list` | `not_directory` | 是 | 是 |
| `create_directory` | 否 | 创建单个目录（父目录须存在） | 禁止根修改 |
| `write/write_text/write_json` | 创建/替换文件 | `not_file` | 禁止根修改 |
| `remove` | 是 | 仅空目录 | 禁止根修改 |

`remove` 不递归。脚本不能创建/删除/替换 `data`、`js` 根。最终候选顶层必须仍是 MZ 的
`data/js`，或 MV Host 内部 `www/data`、`www/js`。没有 `www` 逻辑根。

门面绑定当前候选身份并执行目标、祖先和回滚条件检查；它不是文件系统沙箱。直接修改
`ctx.project.output_root` 会绕过门面，但仍面对最终全量候选验证。

### 10.2 `ctx.write_back.layout`

<!-- att-example: illustrative -->
```lua
ctx.write_back.layout(region, pairs) -> {
  status = "applied" | "manual",
  texts = string[],
  inserted_line_breaks = integer,
  inserted_fullwidth_indents = integer,
}

region = "dialogue_body" | "scrolling_text" | "help_description"
pairs = { { original = string, translation = string | nil }, ... }
```

`translation=nil` 表示冻结原文；它参与跨项状态观察但不得修改。texts 与输入逐项对齐。
`manual` 是正常结果，表示不能安全自动布局，返回无程序新增内容且计数为 0。

WriteBack 没有 validate/discard/publish 或 post-publish 回调。脚本应只依据当前候选和私有
已提交状态作幂等编辑：同一输入重复运行必须得到同一候选。Lua 返回后 Host 全量验证并
发布；Lua 无法在成功发布后再把私有表标记成“已发布”。需要跨运行恢复时，以权威输入和
候选可重建性设计协议，而不是猜测发布结果。

## 11. 错误、取消与副作用

Host 错误以 userdata 抛出，可由 `pcall` 读取：

<!-- att-example: illustrative -->
```lua
error.domain = string
error.kind = string
error.message = string
error.retry_after_ms = integer | nil
```

只按 domain/kind 分支；message 仅诊断。JSON、普通 Host 值、RPG Maker 整文档、Extract
快照、来源和输出转换各自保留不同错误域。常见域包括 `json`、`binding`、
`rpg_maker`、`extract`、`filesystem`、`output`、`sqlite`、`translation`、`llm`、
`runtime`。

当前稳定的常用 kind：

| domain | kind |
|---|---|
| `json` | `invalid_value` |
| `binding` | `invalid_source_path`、`invalid_output_path`、`invalid_value` |
| `rpg_maker` | `invalid_argument`、`invalid_source`、`invalid_plugin_parameter_source`、`invalid_plugins_envelope`、`invalid_location`、`invalid_tag`、`tag_not_found`、`invalid_utf8`、`invalid_json` |
| `extract` | `invalid_standard_snapshot`、`intent_already_declared` |
| `filesystem` | `case_mismatch`、`not_found`、`not_file`、`not_directory`、`invalid_path`、`invalid_utf8`、`io` |
| `output` | `outside_content_roots`、`invalid_path`、`outside_scope`、`scope_root_mutation`、`not_found`、`not_file`、`not_directory`、`directory_not_empty`、`candidate_identity_changed`、`wrong_editor_instance`、`invalid_utf8_name`、`io` |
| `sqlite` | `closed`、`indeterminate`、`transaction_already_active`、`no_active_transaction`、`operation_failed`、`outcome_unknown` |
| `translation` | `prepare`、`accept`、`invalid_state` |
| `llm` | `retryable`、`fatal` |
| `runtime` | `cancelled`、`host_bridge_closed` |

接口参数/值错误和脚本编程错误仍可能由 `binding` 报告；不要通过解析 message 猜测新的
kind。ATT 不按 Lua VM 内存、Host 值字节、节点、深度、错误文本长度或完整文档字节数
提前拒绝；真实分配、地址空间、文件系统、SQLite 或格式失败按其实际 domain/kind 报告。

取消是合作式。Host 调用到达现实副作用后等待明确终态；脚本可捕获取消，但 VM 返回边界
再次观察。完整标准库、本机模块、`os.execute`/`os.exit` 不受同等抢占保证。

失败后的现实状态必须按副作用判断：活动 SQLite 事务由终结器尝试回滚；已提交 SQL 和
已发送 LLM 请求保持；Extract 内存意图只在干净终结后提交；WriteBack 受管修改随候选
丢弃；标准库和外部进程副作用不属于 ATT 自动恢复。
