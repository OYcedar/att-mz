# Manual TOML 人工补译规格

Manual 通过一份可编辑的 UTF-8 TOML，为 MV、MZ 和 Generic 项目补译或修订正文。通常按
“导出 → 填写译文 → 应用”执行；需要单独检查文件时使用 `check`。这些操作使用当前项目数据库，
不请求模型，也不修改 Prompt、术语、语言规则或 Placeholder 配置。

```text
att mv|mz|generic manual export --name NAME \
  [--selection pending|rejected|all | --ids IDS.jsonl] FILE.toml
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
- 数组元素不能包含 CR、LF 或 NUL；译文还不能包含 BOM/U+FEFF。多行内容使用多个数组元素表达；
- 未知字段、标量正文、重复 ID 和其他 TOML 结构均无效。

`fixed` 必须保持数组长度和原文中的必要空槽。原文纯空白槽的译文必须是精确空字符串；
原文非空槽不得译成空字符串或纯空白。`free` 可以按目标语言需要改变数组长度，但非空
原文不得整体译成空白。ATT 不判断译义、源语残留、术语或文风。

## 2. export

`manual export` 原子写入指定文件。默认 `--selection pending`，只导出当前没有有效译文、
没有 Rejected 候选且确实需要翻译的条目。Rejected 表示候选确定违反结构契约，ATT 已保存
候选以避免自动重复请求。

- 不导出空白内容；
- 不导出当前判断为非源语的内容；
- 不导出完全由 Placeholder 保护的内容；
- 不导出已有当前人工或自动译文的内容。

`--selection rejected` 只导出 Rejected。候选能表示为非空字符串数组时预填 `translation`；否则
保留候选 JSON 和确定原因作为注释，`translation = []`，由使用者填写。`--selection all`
导出全部当前条目并预填当前有效译文或可表示的 Rejected 候选。

Rejected 只有在原文、完整 Group 语境和项目语言对仍与候选建立时一致时才属于当前条目。
文件改名、移行等展示位置变化不改变其适用性，导出始终使用当前位置的可读 ID；旧语言对或
旧 Group 语境的候选保留在项目状态中，但不会作为当前 Rejected 导出或预填。

`--ids IDS.jsonl` 按文件中的自然 ID 导出，并预填当前有效译文或可表示的 Rejected 候选。
每行必须且只能是：

```json
{"manual_id":"Actors.json:1:name"}
```

重复、未知或带未知字段的 ID 会使导出失败。导出文件不包含上下文、术语、hash 或数据库
身份；没有选中条目时写出空文件。输出目标必须是普通非 reparse 文件。

RPG Maker 所有权使用独立的 `att mv|mz ownership export` 导出全部 Extract Unit；
`att mv|mz|generic translation export` 导出全部当前 Unit，格式见
[CLI 规格](../runtime/cli.md#5-manual-输出)。

## 3. check

`manual check` 只读取 TOML 和仍有当前可读 ID 的数据库条目。已经失去当前位置的人工记录
与本次文件无关，不参与 `check` 或 `apply`；它们仍由数据库保存，并可通过 Lua 高级接口
检查或清除。`check` 检查：

- TOML 语法和字段闭集；
- ID 是否重复并仍指向当前条目；
- `source` 与当前原文是否逐项一致；
- `type`、数组形状、固定行数和必要空槽；
- 控制字符、RPG Maker 控制码和当前必要 Placeholder；
- 原文非空槽或正文没有被译成空字符串或纯空白。

Placeholder 预期来自当前原文已经建立的 binding。Manual 不要求同一正则在译文标签或自然语言
上下文中再次命中原片段；例如由英文标签 lookbehind 保护的编号，在标签汉化后仍按源文片段、
数量、顺序、wrapper 和槽位验收。无法唯一归属的重复或重叠片段，以及剩余译文中新命中的
Placeholder，仍会报错。

它不检查翻译质量、源语残留、译文是否等于原文、术语、专名或模型能否接受。输出只列出
有效、未填写和错误数量；每个错误说明可读 ID、原因和修改方法。未填写不是错误。

## 4. apply

`manual apply` 在一个数据库事务中执行与 `check` 相同的检查。存在任何错误时不修改任何
条目；全部结构有效时，只应用非空 `translation`，未填写项跳过。

人工译文优先于自动译文。应用时清除同一位置的自动译文和 Rejected 候选，其他位置保持原状态。

后续变化按两类处理：

- 原文、位置、项目语言对或实际写回结构改变，使人工译文不再适用时，旧记录保留为过期正文，
  不作为 Current 导出或写回；
- 适用性仍匹配、但 Placeholder 等强验收契约改变时，按当前契约重新验收。合法正文继续使用；
  违反强不变量的正文及来源转入 Rejected，供人工修订。

术语、文风、语言比例和布局等 Review 不改变人工译文的适用性。完整状态边界见
[SQLite 规格](../runtime/sqlite.md#2-自动译文与人工译文)。

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

批量取得上下文后，在同一份 Manual 中集中填写并应用。少量剩余条目直接补译；只有证据表明
问题来自系统规则时，才修改 Placeholder、语言或其他全局规则。需要重新请求当前 Rejected
条目时，显式使用 Translate 的 `--retry-rejected`。
