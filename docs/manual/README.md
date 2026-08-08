# Manual TOML 人工补译规格

Manual 是 MV、MZ 和 Generic 普通人工补译的统一入口。它只处理当前项目数据库和一份可直接
编辑的 UTF-8 TOML，不请求模型，不修改 Prompt、术语、语言规则或 Placeholder 配置。

```text
att mv|mz|generic manual export --name NAME FILE.toml
att mv|mz|generic manual check --name NAME FILE.toml
att mv|mz|generic manual apply --name NAME FILE.toml
```

`FILE.toml` 是必需位置参数。三个引擎使用相同格式和检查语义。

三个命令都要求发行目录中的固定 `config.toml` 存在且 TOML 语法有效。只有 `export` 读取
其中的语言模块，用于判断哪些当前原文需要翻译；`check` 和 `apply` 不读取或校验语言模块，
也不重新做语言分析。

## 1. 文件格式

文件只包含 `[[translation]]`。每项必须且只能包含以下字段：

```toml
[[translation]]
id = "Skills.json:798:name"
type = "fixed"
source = ["Tails Stomp"]
translation = ["尾击"]

[[translation]]
id = "Map023.json:event17:page1:dialogue42"
type = "free"
source = ["First line.", "Second line."]
translation = ["译文可以按照中文需要重新分行。"]
```

- `id` 是当前项目生成的可读位置，不是 hash、数据库 ID 或编码 locator；
- `type` 只能是 `fixed` 或 `free`；
- `source` 和 `translation` 只能是字符串数组；
- `translation = []` 是唯一的未填写表示；
- 数组元素不能包含 CR、LF 或 NUL，多行内容使用多个数组元素表达；
- 未知字段、标量正文、重复 ID 和其他 TOML 结构均无效。

`fixed` 必须保持数组长度和原文中的必要空槽。`free` 可以按目标语言需要改变数组长度。
非空 `translation` 表示使用者明确填写；ATT 不判断它是否空白、是否等于原文、是否残留
源语或是否符合术语和文风。

## 2. export

`manual export` 原子写入指定文件。默认只导出当前没有有效译文且确实需要翻译的条目：

- 不导出空白内容；
- 不导出当前判断为非源语的内容；
- 不导出完全由 Placeholder 保护的内容；
- 不导出已有当前人工或自动译文的内容。

导出文件不包含上下文、内部状态、失败诊断、术语、hash 或数据库身份。没有待处理条目时
写出空文件。

## 3. check

`manual check` 只读取 TOML 和仍有当前可读 ID 的数据库条目。已经失去当前位置的人工记录
与本次文件无关，不参与 `check` 或 `apply`；它们仍由数据库保存，并可通过 Lua 高级接口
检查或清除。`check` 检查：

- TOML 语法和字段闭集；
- ID 是否重复并仍指向当前条目；
- `source` 与当前原文是否逐项一致；
- `type`、数组形状、固定行数和必要空槽；
- 控制字符、RPG Maker 控制码和当前必要 Placeholder。

它不检查翻译质量、源语残留、译文是否等于原文、术语、专名或模型能否接受。输出只列出
有效、未填写和错误数量；每个错误说明可读 ID、原因和修改方法。未填写不是错误。

## 4. apply

`manual apply` 在一个数据库事务中执行与 `check` 相同的检查。存在任何错误时不修改任何
条目；全部结构有效时，只应用非空 `translation`，未填写项跳过。

人工译文优先于自动译文。应用人工译文只清除同一位置的自动译文，不触发模型请求、全局
规则修改、重新翻译或无关译文失效。人工译文只在对应原文或实际写回结构变化时成为过期；
过期正文继续保存在数据库中，但 WriteBack 不使用它。

术语、Prompt、Profile、Client、语言判断、质量规则或 Placeholder 配置变化不会使已经应用
的人工译文过期。

## 5. 需要上下文时

TOML 不携带上下文。对含义不明的条目，将全部待查 ID 合并到一次 Lua 调用：

```lua
local ids = {
  "Map023.json:event17:page1:dialogue42",
  "Map023.json:event17:page1:dialogue43",
}

local context = ctx.translation.context(ids)
local terms = ctx.terminology.list()
```

不要为每条译文分别启动一次 Lua。少量剩余条目优先使用 Manual；只有证据表明问题属于系统
规则时，才修改 Placeholder、语言或其他全局规则并重新运行 Translate。
