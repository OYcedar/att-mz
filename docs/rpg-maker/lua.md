# RPG Maker Lua 技术参考

本文定义 MV/MZ 向可信 Lua 5.4 主程序公开的当前接口。Lua 用于 Extract、Translate、
WriteBack 的阶段扩展，也可由独立的一次性项目命令执行；Init 没有 Lua。可复制的完整
协议见 [Lua Cookbook](lua-cookbook.md)和 [`examples/`](examples/README.md)。

“可信”不是沙箱：除本机动态模块装载入口外，脚本拥有 Lua 5.4 标准库，并按 ATT
进程的操作系统权限运行。`ctx` 提供的是与冻结来源、翻译语义、SQLite 和未发布候选
相连的受管门面，不是唯一访问路径。

本文所有代码块前使用 `<!-- att-example: valid|invalid|illustrative -->`：valid 是当前 API
可用代码，invalid 是必须失败的反例，illustrative 只展示形状或数据。

## 1. 阶段位置、独立入口与停止线

| 命令 | Lua 位置 | 失败边界 |
|---|---|---|
| Init | 不支持 | 只建立项目与冻结来源 |
| Extract | Builtin → Rules → Lua | 已提交前序 owner 不组合回滚；Lua 意图仅在脚本干净终结后提交 |
| Translate | Standard → Lua | Standard 已提交译文不因 Lua 失败回滚 |
| WriteBack | Standard 候选 → Lua → 验证 → 发布 | Lua 失败丢弃候选；没有发布后回调 |
| Lua | 独立项目程序 | 每次 `standard:accept` 独立提交；后续脚本失败不回滚已成功调用 |

三个阶段入口彼此独立。与一个受信 RPG Maker 位置直接对应的 Standard 单元可在 Extract
用 `replace_standard` 接入 Standard Translate 和 WriteBack；只需由 Lua 声明集合、原子
单元和最终写回关系的文本可以使用可选的 `ctx.translations`，由 ATT 承担翻译协议、调度、
state、增量提交和任务记录。私有 grammar、跨单元原子关系、特殊模型协议或其他高级契约
表达不了的行为继续由 Lua 通过 `ctx.translation`、`ctx.llm`、`ctx.db` 和候选能力完整
拥有。已有可靠人工译文则可从独立 `lua` 命令交给 Standard 核心验收与提交，不必伪造
受管数据库 state。核心不增加通用多目标投影 DSL、自动发布状态或 post-publish hook。

每个阶段独立保存自己的 Lua 主程序快照：非空主程序正文、SHA-256 和无损 Windows 解析
路径。显式提供非空 `--lua` 时精确替换该阶段快照；省略 `--lua` 时，仅在上次成功运行
方案启用了 Lua 的情况下复用；零字节文件显式清除该阶段主程序。自动复用执行数据库中的
正文，不重新读取原文件；保存路径只用于 chunk 名、`require` 搜索目录和诊断。主程序通过
`require`、`io` 或 `os` 动态读取的纯 Lua 模块、文件和进程仍是外部依赖，不纳入快照。

清除语义按阶段区分：显式 Extract 方案未列出 Lua 或提供零字节 Lua 时，同时停用 Lua
Standard owner、删除其标准资产，停用 Lua Managed owner、删除其托管快照，并从后续
自动方案中移除 Lua；Translate 与 WriteBack
只清除各自主程序，不猜测或删除 Lua 私有数据库状态。
三个阶段即使最初来自同一个文件，也仍是彼此独立的快照和运行方案。

独立命令形状为：

<!-- att-example: illustrative -->
```text
att --config FILE mv lua --name NAME [--profile PROFILE_ID] SCRIPT_LUA [-- ARG...]
att --config FILE mz lua --name NAME [--profile PROFILE_ID] SCRIPT_LUA [-- ARG...]
```

它不保存或复用脚本，每次从显式路径重新读取；零字节脚本是合法空程序，主 chunk 返回值
会被忽略。Profile、脚本和参数都不改变任何阶段运行方案。

## 2. VM、连接与 `ctx`

每次阶段调用或独立项目调用都创建新的 OS worker、Lua VM 和 SQLite 连接，主程序结束后
销毁。因此：

- Lua globals、`package.loaded`、闭包、userdata 不跨阶段；
- SQLite TEMP 表、临时 pragma 和连接状态不跨阶段；
- 只有持久数据库表、冻结来源、标准资产或已发布文件能跨阶段交接；
- Extract、Translate、WriteBack 即使保存了同一路径，也不能依赖前一 VM 的内存。

VM 建立后的 `require(name)` 先复用 `package.loaded[name]`。尚未加载时，初始 searcher
按以下顺序查找：

1. `package.preload[name]`；
2. 主程序解析路径所在目录的 `name.lua`、`name/init.lua`；
3. 当前 VM 的 `package.path`，按分号分隔的模板顺序替换 `?`。

主程序目录由独立 searcher 负责；只有其中没有可读模块时才读取 `package.path`，所以
`package.path` 中的同名文件不能覆盖主程序相邻模块。`package.searchpath` 与第三步使用
相同的参数、成功或失败返回形状及 UTF-8 Windows 路径语义；失败结果逐项保留实际候选
路径和 Windows OS code。无效 UTF-8 `package.path` 只在真正进入第三步时失败，不影响
前两个位置已经命中的模块。
Lua 初始化 `package.path` 时优先读取 `LUA_PATH_5_4`，仅在它不存在时读取 `LUA_PATH`；
环境值中的 `;;` 会在该位置插入正式默认路径。变量值和脚本后来写入的模板必须是 UTF-8。
正式 `att.exe` 的默认 `package.path` 包含程序所在目录，并且在中文、Emoji 或空格目录
中仍是有效 UTF-8。进程 cwd 不参与上述模块优先级；相对 `package.path` 模板仍按 cwd
解析。模板也可直接使用本地盘、映射盘、UNC 和长绝对路径；ATT 不做项目根检查，作者
不需要手写 `\\?\` 前缀。

`require` 和 ATT 的文件 searcher 使用 VM 创建时捕获的原始 `package` table。重绑定全局
`package` 不会替换这张表；可信脚本仍可修改 `package.loaded[name]`、
`package.preload[name]` 条目，以及原表的 `package.path` 和 `package.searchers` 字段。
修改 `package.path` 会在下一次真正进入路径 searcher 时生效；已经位于
`package.loaded` 的模块继续复用，清除相应条目后才重新搜索。脚本追加的后续 searcher
会在 ATT 文件 searcher 都未命中后继续执行。

`package.cpath` 和 `package.loadlib` 不公开，ATT 不装载本机 C 模块。`io`、`os` 与
`debug` 开放。正式制品在[受支持的 Windows 环境](../../README.md#运行环境与路径)中，
把以下 Lua string 一律解释为 UTF-8，并无损访问对应 Unicode 路径或环境值：

- `io.open`、`io.input`、`io.output`、`io.lines` 的文件路径；
- `loadfile`、`dofile` 的程序路径；
- `os.remove`、`os.rename` 的文件路径；
- `os.getenv` 的环境变量名和非 nil 返回值。

含未配对 UTF-16 surrogate 的 Windows 名称不能表示为 UTF-8 Lua string，因此不能通过
上述标准库或模块模板直接访问；ATT 只在内部路径与诊断中保留这类名称的 UTF-16 安全身份。

`io.open`、`os.remove` 等标准库失败三元组保留原 UTF-8 路径和 Lua 5.4 规定的
`errno` 第三返回值；错误消息同时包含系统原因和原始 Windows error code。
`os.rename` 的失败消息同时包含源、目标路径，`loadfile`、`dofile` 的失败消息也保留
传入路径。脚本可以据此诊断直接访问，但 ATT 不会把这些返回值转换成受管 Host 错误。

这些函数的相对路径按进程 cwd 解析，不会改按主程序目录解析。`os.execute` 与 `io.popen`
接收 UTF-8 命令字符串，但命令拆分、引用规则、子进程字符编码和退出行为继续由 Windows、
命令解释器与被调用程序决定。脚本调用 `os.setlocale` 主动改变 C locale 后产生的行为不在
初始 UTF-8 运行环境保证内。

上述标准库调用、`loadfile`/`dofile` 动态执行的程序和 `require` 动态读取的模块都属于
直接外部访问。它们可以访问当前进程权限允许的任意 UTF-8 Windows 路径，包括本地盘、
映射盘和 UNC，不受项目根白名单限制。ATT 不冻结其内容，不把写入转成 WriteBack 候选，
不提供 `ctx.source`/`ctx.output` 的逐字大小写与越界检查，也不为它们增加取消、事务、
恢复或候选发布语义。需要这些保证时必须使用受管接口。

<!-- att-example: illustrative -->
```lua
ctx = {
  phase = "extract" | "translate" | "write_back" | "lua",
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
  translations = ManagedTranslationsApi | nil,
  llm = function | nil,
  output = OutputApi | nil,
  write_back = WriteBackApi | nil,
  standard = StandardApi | nil,
}
```

| 阶段 | 非 nil 的阶段接口 |
|---|---|
| Extract | `ctx.extract`、`ctx.translations.replace` |
| Translate | `ctx.translation`、`ctx.llm`、`ctx.translations.translate/open` |
| WriteBack | `ctx.output`、`ctx.write_back`、`ctx.translations.open` |
| Lua | `ctx.standard` |

`json/source/rpg_maker/db` 四种调用都有。`source_root` 是冻结内容物理根：MZ 对应 `source/`，
MV 对应 `source/www/`。`output_root` 仅 WriteBack 存在且是未发布候选物理路径。这些物理
路径是 UTF-8 Lua string，可供可信脚本诊断或显式传给上述标准库；受管 API 一律使用
下一节逻辑路径。

独立命令中 `ctx.extract/translations/translation/llm/output/write_back` 都是 nil。全局
`arg[0]` 是解析后的主脚本路径，`arg[1..]` 是 `--` 后按顺序传入的 UTF-8 参数；不能表示
为 UTF-8 的参数在脚本运行前显式失败。阶段 Lua 不建立这份独立命令参数契约。

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
控制字符、非 UTF-8，以及任一路径段末尾的点或空格。Host 从 `data`/`js` 根开始逐段解析，
每个中间目录和最终文件/目录都必须与实际目录项逐字同大小写；请求
`data/items.json` 而实际为 `data/Items.json` 时抛出 `filesystem/case_mismatch`，不借助
Windows 大小写别名。真实不存在仍按具体操作返回既有 `filesystem/not_found`。

在来源已经冻结、候选只经本 Host 编辑的单次 Lua 执行边界内，Host 按受检目录路径缓存
`ctx.source` 与 `ctx.output` 的成功列举；失败列举不缓存，下一次 Lua 执行也重新观测。
`ctx.output.write` 每次真正尝试后使父目录与目标路径失效；`create_directory` 可能逐段
建立缺失目录，因此使根与目标路径上的每一级前缀失效；`remove` 使父目录、目标及全部
后代失效。即使候选编辑操作返回错误也执行相同失效；失效代还会阻止较早开始的列举在
完成后把旧结果重新放回缓存。缓存只减少重复目录列举，不改变逐字大小写检查、越界检查
或具体操作的错误文本。

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

## 5. `ctx.rpg_maker` 来源、路径与完整 Value

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
或随后读取。`location`/`text` 由 Host 建立不可伪造的完整 JSON Value 身份，显示字符串只
用于诊断。`document:text` 要求路径终点本身是 string；引用的 `original` 是这个 string 的
全部字节。裸 `<`、`>`、`:` 以及形如 `<Help:炎之剑的说明>` 的内容都没有 Host 级语法。

路径可以包含一个或多个 `DECODE_JSON`。Standard WriteBack 会沿相同路径逐层解码，验收
完整 Value 的冻结原文后替换整个 string，再从内向外编码为紧凑 JSON string；任一层失败
都不会提交该文档候选。被解码层原有的 JSON 排版空白不会保留，未修改的 JSON 值与
UTF-8 文本保留。

插件私有标签、注释块或其他 grammar 由脚本完整拥有：Extract 读取原始 Value 后自行解析
并建立私有身份；Translate 可按明确 kind 主动调用 `prepare/accept`；WriteBack 复核当前
完整原值，按私有 grammar 重建，再写回完整 Value。Host 不提供标签扫描、occurrence、
局部拼接或私有 grammar 验收。

<!-- att-example: valid -->
```lua
local entry = ctx.json.array({ 1 })
local note = document:text(ctx.json.array({ 1, "note" }))
assert(note.original == "<Help:炎之剑的说明>")
```

## 6. Extract：`replace_standard`

### 6.1 `replace_standard`

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
| `database_entry` | 除 System 外的标准 Data、自定义 DataFile、Map | Value |
| `system` | 仅标准 `System.json` | Value |
| `map` | 仅规范 Map | Value |
| `event_command` | 标准 Data、自定义 DataFile、Map | Value |
| `plugin_parameter` | 显式 PluginParameter source | Value |

`location` 与每个 field 必须来自同一 source 的 Value。组位置负责语义上下文和顺序，
field 的完整 Value 才是最小翻译单元与写回目标。Lua 不通过此接口构造 Dialogue、
Choices、ScrollingText、局部标签投影或一对多投影。

`groups` 和 `fields` 都是从 1 开始无洞数组；声明顺序分别成为 `group_order` 和
`unit_order`，不得按字母重排。同一次调用中 `group.location` 本身必须唯一；同位置同 kind
或不同 kind 都是 `extract / invalid_standard_snapshot`，Host 不会把它们自动合并。每组至少
一个 field；name 非空且组内唯一；原文不能全空白；逻辑身份和 Mutation Claim 不得冲突。

一次脚本最多声明一个 Standard 意图：一次 `replace_standard` 或 `clear_standard`。第二次失败。
`replace_standard({})` 是 active 的空 Lua owner；`clear_standard()` 停用 owner；不调用
表示保持旧 Standard owner。它与下一节的 Managed 意图彼此独立，同一脚本可以各声明一次。

意图先完整校验并留在内存。只有脚本正常返回、未取消、SQLite 连接干净终结且无活动事务
时，Host 才进入 Store 提交。脚本通过 `ctx.db` 的提交不和 Store 事务合并。

### 6.2 `ctx.translations.replace`

<!-- att-example: valid -->
```lua
ctx.translations.replace({
  {
    name = "quest_titles",
    instruction = "翻译任务标题；保持简洁。",
    units = {
      {
        key = "quest:arrival",
        kind = "plugin_parameter",
        shape = "single",
        original = "星港へ",
        context = "",
        metadata = ctx.json.object({ quest_id = 12 }),
      },
    },
  },
})
```

从真实 JSON 字段声明 unit、执行托管翻译并按 metadata 写回同一字段的完整主程序，见
[Managed 三阶段示例](lua-cookbook.md#2-managed-三阶段翻译)。

`replace(collections)` 一次性声明 Lua Managed owner 的完整当前快照。一次 Extract 主程序
最多调用一次；`replace({})` 保持 owner active 并清空全部 collection；不调用表示保持旧
Managed 快照。零字节 Extract Lua 或 Extract 运行方案停用 Lua 时，Managed owner 和
Standard Lua owner 一起停用，不留下可供 Translate 或 WriteBack 打开的托管快照。

外层、`units` 和所有数组正文都必须是从 1 开始、无洞且没有其他 key 的稠密数组。
collection 只接受 `name`、`instruction`、`units`；unit 只接受 `key`、`kind`、`shape`、
`original`、`context` 和可选 `metadata`。缺失字段、未知字段、错误类型、非 UTF-8
字符串、空白 `name/key/kind`、重复 collection `name` 或同一 collection 内重复 `key`
立即使声明失败；不同 collection 可以使用相同 `key`。`instruction` 与 `context` 是必填
字符串并允许 `""`。

`kind` 精确使用 `database_entry`、`system`、`map`、`dialogue`、`choices`、
`scrolling_text`、`event_command`、`plugin_parameter` 之一，选择对应的 RPG Maker
Placeholder 与语言验收语义。四种原子 `shape` 为：

| shape | `original` | 模型与验收结果 |
|---|---|---|
| `single` | 标量字符串，不得含 CR、LF 或 NUL | 一个模型 ID；JSON 值是单元素字符串数组，译文不得含 LF |
| `reflow` | 标量字符串，不得含 CR 或 NUL | 一个模型 ID；JSON 值是单元素字符串数组，元素可含 LF |
| `lines` | 非空稠密字符串数组；元素不得含 CR、LF 或 NUL | 一个模型 ID；项数及空槽位置不变，Placeholder 可按 Lines 规则在同一 ID 内移动 |
| `items` | 非空稠密字符串数组；每项非空白且不得含 CR、LF 或 NUL | 一个模型 ID；项数不变且每项独立验收，Placeholder 不得跨位置移动 |

数组整体是一个不可拆分的翻译、验收和提交原子；当前契约不提供跨 unit 原子组。
`metadata` 若存在，必须是 `ctx.json` 能无损表达的显式 JSON 值；object/array table
仍须分别由 `ctx.json.object(...)` / `ctx.json.array(...)` 建立，JSON null 使用
`ctx.json.NULL`。Host 将它作为不透明 JSON 原样随 unit 持久化并在后续 Lua 中还原；
它不发送给模型，不参与 state、去重或译文保留判断。Lua `nil` 表示未声明 metadata，
与显式 JSON null 不同。

脚本正常返回前，Standard 与 Managed 声明都只存在内存。Host 先完整验证二者，确认未
取消且交互数据库干净终结后，再在同一个 Store 事务中原子应用两个意图；任一意图失败都
不会提交另一意图。仅 `metadata` 或自然顺序变化时保留同身份 unit 的 translation/state；
`instruction`、`kind`、`shape`、`original` 或 `context` 改变时对应译文失效，删除 collection
或 unit 会删除对应托管状态。

## 7. Translate：prepare、Current 与 accept

### 7.1 低级标量接口

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
state/Prompt 共用结果。Custom Placeholder 的 `scopes` 只比较本次传入的 kind；同 kind
时与文本来自 Builtin、Rules 还是 Lua 私有协议无关，异 kind 不消费该规则。Lua 只有主动
调用 `prepare(kind, ...)` 才消费这些规则。`prepare` 不读写脚本私有译文，也不验收脚本
自己的标签 grammar。

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

Standard 多 ID/Lines 响应的结构错误不属于 Lua 标量 accept 的 reason 集合。裸 `<` 与
`>` 是普通候选内容；Lua 标量 accept、Standard 模型响应和 `standard:accept` 都不会按
相似插件语法追加启发式禁令。只有 MV/MZ Builtin 控制符和当前 Custom Placeholder 明确
匹配的跨度会被当作 opaque 内容保护。

成功 acceptance 的 translation/state 必须由脚本在同一 SQLite 事务中成对写入私有表。
核心不替 Lua 选择身份或事务粒度。

`non_source_language` 与 `fully_protected` 没有可接受的 candidate；推荐成对删除旧
translation/state 并保持 NULL。以后再次运行 Translate 会重新 `prepare`，但不会请求
LLM。Lua 私有 Current 也只在 Translate 脚本实际调用 `is_current` 时检查：任何已绑定语义
变化后，都必须先重新运行同一 Translate 脚本，再进入 WriteBack；核心不会替私有协议
读取或刷新其表。

### 7.2 `ctx.translations.translate/open`

<!-- att-example: valid -->
```lua
local report = ctx.translations.translate()
for result in report:units() do
  print(result.collection, result.key, result.status)
  -- result.translation、result.reason、result.details
end

local collection = ctx.translations.open("quest_titles")
if collection ~= nil then
  local arrival = collection:get("quest:arrival")
  for unit in collection:units() do
    -- key、kind、shape、original、context、metadata、translation、status
  end
end
```

同一脚本怎样在 Translate 读取报告、在 WriteBack 直接打开最后已确认状态，见
[Managed 三阶段示例](lua-cookbook.md#2-managed-三阶段翻译)。

`translate()` 无参数，一次 Translate 主程序最多调用一次。它读取一致的完整 Managed
快照，一次处理全部 collection，并为它们建立共同的 Managed 全局去重域；空快照返回
零项报告且不请求模型。调用前若 `ctx.db` 有活动事务，则以
`translations/transaction_conflict` 在零模型请求、零托管修改的边界失败。Lua 应先结束
自己的事务，再让 Host 进入会持续跨并发请求和多个短提交事务的托管执行。

报告的 `units()` 按 collection、unit 自然顺序迭代一次。每项提供：

- `collection`、`key`：只用于把结果映射回 Lua 声明身份；
- `status`：`current`、`translated`、`not_applicable` 或 `unavailable`；
- `translation`：`single/reflow` 返回标量 string，`lines/items` 返回带 JSON array 标记的
  稠密 string 数组；没有可用译文时为 nil；
- `reason`：存在正常未产出或拒绝时的稳定原因，否则为 nil；
- `details`：存在时为 `ctx.json.object`，否则为 nil。

报告是本次执行结果，不是权威存储。`translate()` 正常返回后，Translate 阶段才允许
`open(name)`；调用前使用 `open` 明确失败。WriteBack 阶段不需要先调用 `translate()`，
可以直接打开最后已确认提交的快照。不存在的 collection 返回 nil；`name` 必须是非空
UTF-8 字符串。

`open` 返回只读 collection userdata：`name`、`instruction` 是字段，
`get(key)` 精确查找并在不存在时返回 nil，`units()` 按声明顺序单次迭代。只读 unit
提供 `key`、`kind`、`shape`、`original`、`context`、`metadata`、`translation` 和
`status`。Translate 在本轮 `translate()` 后打开时，状态为 `current`、`not_applicable`
或 `unavailable`，其中刚验收的 `translated` 报告结果投影为 `current`；WriteBack 从
持久快照打开时只投影 `current` 或 `missing`，不会保存某轮正常未产出的具体结果。来源
快照已经改变时，`open` 会明确失败，不把 stale 伪装成可读单元状态。
标量/数组投影与报告相同，`metadata` 仍为原来的不透明 JSON 值。

打开 collection 不会修改游戏资产，也不会替 Lua推导写回位置。WriteBack 必须使用
`ctx.rpg_maker` 重新核对完整来源关系，再用 `ctx.output` 把当前译文写入未发布候选。
项目冻结来源与 Managed owner 记录的来源不一致时，Translate 和 WriteBack 都明确报告
stale，不提供陈旧译文继续运行。

Managed 路径由 ATT 拥有全局去重、临时 ID、装箱、JSON/可选 `<why>`、并发、网络重试、
逐 ID 验收、state、增量提交和 task-records。它不会自动调用低级接口，也不会在某个
unit 无法表达时自动降级。需要私有 grammar、跨 unit 原子关系、特殊模型协议或自有状态
机时，脚本显式使用本节 7.1、`ctx.llm` 与 `ctx.db`；两套接口可以在同一 Translate
主程序中共存。Host 不提供通用 `llm.batch`。

## 8. 独立项目 Lua：Standard 人工译文验收与提交

`ctx.standard` 只在独立 `lua` 命令中存在。它把已经由人或其他受信来源准备好的候选交给
Standard 核心，不发送 LLM 请求，也不暴露内部 state：

<!-- att-example: valid -->
```lua
assert(ctx.phase == "lua")
assert(ctx.extract == nil and ctx.translation == nil and ctx.translations == nil and ctx.llm == nil)
assert(ctx.output == nil and ctx.write_back == nil)

local standard = ctx.standard.open()
for unit in standard:units() do
  print(unit.owner, unit.group_kind, unit.role.kind, unit.status)
end
```

显式 `--profile` 精确选择当前配置中的 Profile。未显式指定时，`open()` 才复用项目上次
成功 Translate 保存的 Profile；没有保存 ID，或该 ID 已不在当前配置中时，只有 `open()`
失败，不使用 Standard 的普通项目 Lua 仍可执行。选择只作用于当前程序，不替换 Translate
方案。会话固定读取项目当前 canonical 术语和 Placeholder，不接受临时资源覆盖。

### 8.1 会话与只读 `StandardUnit`

`standard:units()` 按 Standard 持久自然顺序遍历全部物理单元。会话打开和枚举都是只读
操作，不清除 stale 译文，也不自动传播已有 Current。

已持有真实 `RpgMakerLocation` userdata 时，可以按完整身份精确查找：

<!-- att-example: valid -->
```lua
local item = ctx.rpg_maker.open(ctx.rpg_maker.data("Items.json"))
local unit = standard:get("builtin", item:location({ 1 }), {
  kind = "scalar",
  field = "description",
})
```

owner 只接受 `builtin`、`rules`、`lua`。role 的结构化形状是
`{kind="scalar", field=FIELD}`，或只有 kind 的 `dialogue_speaker`、
`dialogue_body`、`choices`、`scrolling_text`。位置、owner 或 role 没有精确命中时返回
nil；不接受数据库位置 JSON、展示字符串或自造 userdata。

每个 `StandardUnit` 是当前会话创建的只读 userdata。它不能在 Lua 中构造，来自其他会话
的句柄不能传给本会话 `accept`。字段为：

| 字段 | 形状与含义 |
|---|---|
| `owner` | `builtin`、`rules` 或 `lua` |
| `group_kind` | Standard 组 kind |
| `group_location` | 只读 `RpgMakerLocation` userdata |
| `role` | 上述结构化 role table |
| `original` | Value 为 string；Lines 为无洞字符串数组 |
| `source_context` | Standard 完整源上下文的无损 JSON→Lua 投影 |
| `translation` | 当前保存译文，形状同 original；不存在时 nil |
| `model_text` | Placeholder 处理后的候选输入，形状同 original |
| `terms` | 有序 `{term=..., translation=...}` 数组 |
| `content_kind` | `value` 或 `lines` |
| `line_policy` | `single`、`aligned` 或 `reflow` |
| `expected_line_count` | `single` 为 1，`aligned` 为严格槽数，`reflow` 为 nil |
| `status` | `current`、`missing`、`not_applicable` 或 `unavailable` |
| `family_size` | 本次验收可能传播的物理位置数 |

`unavailable` 表示该物理单元无法建立完整 Standard 准备语义；它仍可用于调查，但候选只会
正常拒绝。`not_applicable` 表示原文不是源语言或全部被 Placeholder 保护。

### 8.2 候选形状与正常拒绝

<!-- att-example: valid -->
```lua
local results = standard:accept({
  {
    unit = unit,
    candidate = "人工译文",
    replace_current = false,
  },
})

if results[1].accepted then
  print(results[1].translation, results[1].changed_locations)
else
  print(results[1].reason)
end
```

batch 必须是无洞候选数组，结果与输入等长。每项只接受 `unit`、`candidate` 和可省略的
`replace_current`；省略等于 false。Value 候选必须是 UTF-8 string，Lines 候选必须是无洞
UTF-8 字符串数组，不能把含 LF 的标量冒充 Lines。

`single` 拒绝换行；`aligned` 要求与原 Lines 保持槽数及每个空槽；`reflow` 用于
DialogueBody，候选仍是 Lines 数组，但允许数组长度改变。候选按 `model_text` 中的
Standard ATT token 编写；核心验收后恢复真实控制符并返回规范译文。

成功项为：

<!-- att-example: illustrative -->
```lua
{ accepted = true, translation = "规范译文", changed_locations = 2 }
```

正常候选拒绝不抛异常、不写库，返回
`{accepted=false, reason=STABLE_CODE, ...结构化详情}`。除普通 Standard 候选拒绝代码外，
人工入口增加：

| reason | 含义 |
|---|---|
| `not_applicable` | 单元不需要翻译 |
| `unavailable` | 该单元无法建立验收语义；详情说明原因 |
| `conflicting_candidate` | 同批同一去重族的候选或覆盖选项不一致 |
| `current_replacement_required` | 候选会改变至少一个 Current 成员，但未明确允许覆盖 |

逐行拒绝详情中的 `line` 使用 Lua 1-based 行号；`expected`、`actual`、token、fragment 等
详情只在对应事实存在时返回，不应通过解析 message 补猜。

同批同一去重族的完全相同候选和 `replace_current` 选项只验收、提交一次，每个输入位置仍
得到自己的等长结果；二者任一不同则该族在本批出现的全部项都以
`conflicting_candidate` 拒绝。已有 Current 与候选规范译文相同时按幂等成功处理，并可补齐
同族 missing/stale 位置。候选会改变任一 Current 成员时，必须为该族明确设置
`replace_current=true`；通过后整个族同步替换。

### 8.3 原子性、并发与恢复

每次 `standard:accept(batch)` 先完成所有普通验收。被正常拒绝的项排除，全部合法族进入
同一个短 SQLite 事务。事务内核心重新检查项目 source snapshot、全部 owner/resource
fingerprint，以及每个传播位置的完整身份、原文、源上下文和打开会话时的
translation/state pair；代表项和每个传播位置分别计算正确 state，并用 CAS 成对写入。

任一目标已经变化时，全部合法族回滚并抛 `standard/stale_snapshot`；重新
`ctx.standard.open()` 取得新会话后再重新判断候选。明确 SQLite 失败和提交终态未知继续
使用结构化 SQLite 错误，不能当成普通拒绝。成功返回时该批已经提交，脚本之后失败、取消
或另一次 accept 失败都不会回滚此前成功调用；同一会话后续重新枚举或 `get` 得到的投影
会同步为新 Current，已经交给 Lua 的旧 userdata 仍是调用前的只读快照。

活动 `ctx.db` 事务中调用 `accept` 会抛 `standard/transaction_conflict`。会话打开后，脚本
若通过 SQL 修改相关权威状态，后续 accept 会由 CAS 判为 `standard/stale_snapshot`。脚本
退出时未关闭的 `ctx.db` 事务仍按公共终结协议回滚。不要直接 SQL 修改 ATT 受管翻译表：
schema 不是 Lua 契约，绕过核心会破坏 translation/state 配对与 Current 含义。

## 9. `ctx.llm`

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

`ctx.llm` 不生成翻译任务记录。Lua 拥有任意私有消息协议、验收、身份和数据库
事务，核心无法从一次 Provider 响应推导脚本的逐 ID 结果或最终提交终态，因此不会把
低级调用伪装成 ATT 托管 TaskBlock。Lua 排障使用运行级 JSONL 摘要、脚本自己的稳定诊断
和私有状态证据。

## 10. SQLite：私有协议与事务

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

## 11. WriteBack 候选与布局

### 11.1 `ctx.output`

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

### 11.2 `ctx.write_back.layout`

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

### 11.3 托管译文写回

WriteBack 的 `ctx.translations` 只提供 `open(name)`，不提供 `replace` 或 `translate`。
脚本读取最后已确认提交的 collection/unit；有译文时投影为 `current`，否则为
`missing`。脚本用声明时保存的 `key`、`metadata` 和自身确定关系找到候选目标，并通过
`ctx.output` 完成写回。某轮 Translate 的 `not_applicable` 或 `unavailable` 只在该轮报告
和同进程投影中存在，不进入 WriteBack 快照。Host 不把 Managed unit 自动变成 Standard
recipe 或 Mutation Claim，也不替脚本决定 `missing` 应如何影响私有资产。来源 stale 在
打开 collection 时明确失败，不把旧译文交给脚本继续发布。

可直接执行的完整目标映射与来源漂移检查，见
[Managed 三阶段示例](lua-cookbook.md#2-managed-三阶段翻译)。

## 12. 错误、取消与副作用

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
`rpg_maker`、`extract`、`filesystem`、`output`、`sqlite`、`translation`、`standard`、
`translations`、`llm`、`runtime`。

当前稳定的常用 kind：

| domain | kind |
|---|---|
| `json` | `invalid_value` |
| `binding` | `invalid_source_path`、`invalid_output_path`、`invalid_value` |
| `rpg_maker` | `invalid_argument`、`invalid_source`、`invalid_plugin_parameter_source`、`invalid_plugins_envelope`、`invalid_location`、`invalid_utf8`、`invalid_json` |
| `extract` | `invalid_standard_snapshot`、`intent_already_declared` |
| `filesystem` | `case_mismatch`、`not_found`、`not_file`、`not_directory`、`invalid_path`、`invalid_utf8`、`io` |
| `output` | `outside_content_roots`、`invalid_path`、`outside_scope`、`scope_root_mutation`、`not_found`、`not_file`、`not_directory`、`directory_not_empty`、`candidate_identity_changed`、`wrong_editor_instance`、`invalid_utf8_name`、`io` |
| `sqlite` | `closed`、`indeterminate`、`transaction_already_active`、`no_active_transaction`、`operation_failed`、`outcome_unknown` |
| `translation` | `prepare`、`accept`、`invalid_state` |
| `standard` | `invalid_argument`、`invalid_role`、`foreign_unit`、`transaction_conflict`、`stale_snapshot`、`invalid_result`、`profile_required`、`saved_profile_unavailable`、`profile_invalid`、`profile_state_unavailable`、`profile_resources_invalid`、`snapshot_unavailable`、`open_failed`、`acceptance_failed`、`internal_invariant` |
| `translations` | `invalid_snapshot`、`intent_already_declared`、`invalid_argument`、`already_translated`、`translate_required`、`transaction_conflict`、`stale_snapshot`、`unavailable` |
| `llm` | `retryable`、`fatal` |
| `runtime` | `cancelled`、`host_bridge_closed` |

主程序或模块读取、编译失败，以及未被脚本捕获的 VM 错误，会进入 ATT 的安全诊断并保留
命令与阶段、主程序或模块路径、Lua 原因，以及实际存在的 Windows 或文件系统错误码。有效
Unicode 路径直接显示；Lua string 无法表示的内部 Windows 路径使用 UTF-16 安全身份。
顶层不会把这些事实压缩成没有原因和路径的“Lua 主程序运行失败”。

接口参数/值错误和脚本编程错误仍可能由 `binding` 报告；不要通过解析 message 猜测新的
kind。ATT 不按 Lua VM 内存、Host 值字节、节点、深度、错误文本长度或完整文档字节数
提前拒绝；真实分配、地址空间、文件系统、SQLite 或格式失败按其实际 domain/kind 报告。

取消是合作式。Host 调用到达现实副作用后等待明确终态；脚本可捕获取消，但 VM 返回边界
再次观察。`os.execute`/`os.exit` 和被脚本替换的调试 hook 不受同等抢占保证。

失败后的现实状态必须按副作用判断：活动 SQLite 事务由终结器尝试回滚；已提交 SQL 和
已发送 LLM 请求保持；Extract 内存意图只在干净终结后提交；WriteBack 受管修改随候选
丢弃；已经成功返回的 Standard 人工提交保持；标准库和外部进程副作用不属于 ATT 自动
恢复。
