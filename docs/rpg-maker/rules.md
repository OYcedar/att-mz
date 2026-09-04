# RPG Maker 规则文件现行规格与编写指南

RPG Maker MV/MZ 的三类声明式规则，其引擎专用事实由本规格统一约定：

- MV 对话姓名投影（`--dialogue-rules`）；
- Extract Rules（`extract --rules`）；
- Placeholder Rules 的 RPG Maker kind、Builtin 控制符与数组槽边界
  （`translate --placeholders`）。

MV 姓名投影与 Extract Rules 的字段、默认值、互斥关系、解析失败和执行失败，均以本
规格为唯一权威。Placeholder 的公共 TOML、捕获、token、恢复与资源生命周期以
[公共 Placeholder 规格](../translation/placeholders.md)为准；本规格只补充 MV/MZ
作用域、控制符和形状规则。提取、翻译阶段的状态交接分别见
[Extract](extraction.md)与[Translate](translation.md)。

WriteBack 的排版规则是另一份跨引擎文件，不属于本规格的三类提取/翻译规则；其严格 TOML、
选择器、项目持久化和 401/405/LF 物化只由
[WriteBack 排版规则规格](../translation/write-back-layout-rules.md)定义。

## 1. 三类文件解决不同问题

| 文件 | 命令位置 | 它拥有的事实 | 它不负责 |
|---|---|---|---|
| MV 姓名投影 | `mv extract --builtin --dialogue-rules FILE` | 标准 MV 对话第一条 `401` 中的 Speaker 投影 | 插件消息、任意文本提取、控制符保护 |
| Extract Rules | `mv|mz extract --rules FILE` | 已知来源、确定路径、最终字符串及可逆写回边界 | 猜测可见性、跨文档关系、多目标同步 |
| Placeholder Rules | `mv|mz translate ... --placeholders FILE` | 已提取文本中不可让模型改写的协议跨度 | 新增提取位置、修复错误分组 |

真实关系需要动态键枚举、条件筛选、跨文档判断或一个译文写到多个目标，而 Rules 无法
完整表达时，由了解该格式的外部操作者转换成
[Generic JSONL](../generic/jsonl.md)，并使用独立 Generic 项目。

## 2. 共同根结构、严格解析与生命周期

三类文件都是严格 UTF-8 TOML，根恰好包含 `rule` 数组。非空定义使用一个或多个
`[[rule]]`；清空定义写作：

```toml
rule = []
```

零字节文件、只有注释的文件、缺少 `rule`、未知字段、重复字段或错误类型，都按普通
无效输入处理，而不是空定义。文件只接受本规格列出的根、字段和值。

```toml
# 缺少必需的 rule 根；注释文件不是空定义。
```

文件参数的生命周期如下：

| 输入方式 | MV 姓名投影 | Extract Rules | Placeholder Rules |
|---|---|---|---|
| 提供非空文件 | 完整替换项目姓名定义，并与本次 Builtin 一起执行 | 完整执行并原子替换 Rules owner | 完整替换自定义占位符资源 |
| 提供 `rule = []` | 清空姓名定义 | 停用并删除 Rules owner 快照 | 清空自定义规则；Builtin 保护仍在 |
| 省略参数 | 复用项目当前定义，不重新解析文件 | 所有 owner 参数均省略时按上次成功方案复用；显式选择其他 owner 时不执行且既有资产不变 | 复用项目当前资源 |

`--dialogue-rules` 必须与 MV `--builtin` 同时提供。候选失败时，对应的旧定义或快照保持
不变；其他 owner 已经成功提交的结果继续保留。阶段之间的提交顺序见
[提取规格](extraction.md)和[翻译规格](translation.md)。

Extract Rules 的 `rule = []` 成功生效后，CLI 与项目日志使用同一份四字段诊断说明停用、
资产删除和运行方案影响；它是退出码仍为 `0` 的成功警告，不是无效规则错误。

每条非空 Extract Rule 的数组位置就是其从 1 开始的自然规则序号。规则物化出的每个 Unit
都保存这个序号；重新 Extract 时按当前 TOML 重新建立，不从字段路径、匹配次序或旧数据库
推断。`ownership export` 只读取这项已保存事实，因此 Rules TOML 与外部规则清单
可以用同一个自然序号逐条核对。

### 2.1 PCRE2 与三层转义

三类规则的正则都使用 PCRE2，开启 UTF 与 UCP。写法上它和 JavaScript `RegExp` 不同：
标志写在模式内（如 `(?i)`），不用 `/.../flags` 字面量；Unicode 属性、命名捕获、锚点
和替换语义以 PCRE2 为准。`.` 默认不匹配 LF；想让单次匹配跨越多行，就在模式中显式
启用 DOTALL，例如 `(?s)`。DOTALL 只决定 PCRE2 能否看到 LF，Placeholder 的 `Lines`
槽边界依然有效：匹配可以跨过 `Value` 内部的 LF，但实际 opaque 保护跨度不得吞入两个
`Lines` 元素之间的拼接 LF。

写 pattern 时推荐用 TOML 单引号字面字符串，让反斜杠原样传给 PCRE2。下面两个完整例子
使用 Placeholder Rules 格式，因此都提供必填的 `order`。MV 姓名规则与 Extract Rules
遵循相同的转义原则，各自还需满足 `speaker` 捕获或 Extract 来源、`text` 捕获要求。

```toml
[[rule]]
order = 'preserve'
pattern = '\A(?<text>正文)\z'
```

写 Extract 路径里的 quoted key 时，最多有三层语法叠在一起，逐层算清才不会出错：

1. TOML 字符串；
2. 路径中 quoted key 使用的 JSON string；
3. `pattern` 才有 PCRE2 转义。

例如，键的实际字节是 `a"b`（字母、双引号、字母）时，使用 TOML 单引号保住路径文本，
再按 JSON string 把双引号写成 `\"`：

```toml
path = '["a\"b"]'
```

匹配游戏文本中的字面 `\SE[Bell]`，PCRE2 仍需用 `\\` 匹配一个反斜杠：

```toml
[[rule]]
order = 'preserve'
pattern = '\\SE\[[^]]+\]'
```

## 3. MV 对话姓名投影

### 3.1 完整根结构与字段

每项只有一个字段：

| 字段 | 类型 | 必填 | 默认值 | 约束 |
|---|---|---:|---|---|
| `pattern` | string | 是 | 无 | 非空 PCRE2；必须恰好有一个名为 `speaker` 的命名捕获 |

```toml
[[rule]]
pattern = '\A\\N<(?<speaker>[^>]*)>\z'
```

可以使用未命名捕获；命名捕获只允许一个 `speaker`。下面的空模式会在编译时被拒绝：

```toml
[[rule]]
pattern = ''
```

### 3.2 类型、默认值与互斥

`pattern` 只接受 TOML string，没有默认值，也是每项唯一允许的字段；缺少它、写成数组、
混入 Extract/Placeholder 字段或出现未知字段，整份文件都会无效。`speaker` 命名捕获必须
恰好一个，其他命名捕获一律非法。

姓名定义只为 MV Builtin 服务；MZ 有自己的原生 Speaker，这份文件对它完全不生效。

### 3.3 解析失败

出现下列任一问题，整份定义在读取或编译候选时就会被拒绝，冻结游戏来源不会被触碰：
根不是 `rule` 数组、字段缺失/重复/未知、类型错误、空模式、无效 PCRE2、`speaker` 数量
不为一，或存在其他命名捕获。PCRE2 与转义边界以[第 2.1 节](#21-pcre2-与三层转义)为准。

### 3.4 针对来源执行失败

姓名投影只扫描标准 MV 事件消息块 `101 + 连续 401*` 的第一条
`401.parameters[0]`。MZ 使用 `101.parameters[4]` 的原生 Speaker，不读取此文件。

每个完整匹配必须非零宽、位于 UTF-8 字符边界；`speaker` 必须参与并完全位于该匹配内。
`speaker` 捕获本身允许空字符串或纯空白。ATT 仅用 `speaker.trim().is_empty()` 判断是否
建立 Speaker；一旦建立，捕获的原始字节（包括首尾空白）原样保存、比较和写回，不 trim、
不规范化。单次空/纯空白捕获是合法命中；但一条非空规则在整份当前冻结来源执行完后，
仍必须至少建立过一个非空白 Speaker，证明这条规则在本游戏中有现实消费。

一条规则可以在同一第一行产生多个不重叠匹配，但所有实际建立的 Speaker 必须原始字节
完全相同。两条不同规则只要都命中同一第一条 `401` 就冲突——跨度是否重叠、谁先写都
不影响判定。规则序号只用于错误定位。

ATT 将从行首到最后一个 marker 结束处投影成 Literal/SpeakerSlot。最后 marker 后：

- 含任一非空白字符：整个后缀成为该行 Body；
- 为空或纯空白：后缀冻结成 Literal，不建立 Body。

后续连续 `401` 仍组成同一 DialogueBody。空或纯空白 `speaker` 只冻结已匹配外壳，不建立
Speaker。

```text
源第一行：\N< 勇者 >   你好
pattern ：\A\\N<(?<speaker>[^>]*)>
Speaker ：" 勇者 "        # 原始空格保留
Body    ："   你好"        # 后缀含非空白，整体进入 Body

源第一行："\N<>   "（引号不属于原文）
Speaker ：不存在           # 捕获参与但 trim 后为空
Literal ："\N<>   "       # 纯空白后缀也冻结
Body    ：不由这一行建立
```

### 3.5 原子失败范围

解析/编译失败会拒绝整份定义。针对冻结来源执行时，下列任一事实拒绝本次 Builtin 候选，
旧姓名定义和旧 Builtin 快照保持不变：

- 完整匹配零宽、越界或不在 UTF-8 边界；
- `speaker` 未参与、越出完整匹配或不在 UTF-8 边界；
- 同一行中同一规则建立了两个原始字节不同的 Speaker；
- 两条规则命中同一第一条 `401`；
- 某条规则在全部当前对话中从未建立任何非空白 Speaker；
- 投影与 Builtin/Rules 的物理修改声明冲突。

按当前游戏的姓名显示协议确定 marker 和锚点，并用形似但不应提取的文本验证边界。

### 3.6 提供文件、略去参数与显式空数组的生命周期

- 提供非空 `--dialogue-rules FILE`：完整替换项目姓名定义，并与本次 MV Builtin 一起执行；
- 省略 `--dialogue-rules`：复用项目当前定义，不重新解析外部文件；
- 提供内容为 `rule = []` 的文件：明确清空项目姓名定义。

该参数只允许与 `mv extract --builtin` 同时使用。候选失败时，旧姓名定义和旧 Builtin
快照都保持不变。

### 3.7 可复制正例、反例和物化结果

本节 3.1 的两个 TOML 是最小完整正例和反例，3.4 给出了 Literal、SpeakerSlot 与 Body
的逐字物化结果。可直接复制并由生产解析器读取的多规则文件见
[`examples/mv-dialogue.toml`](examples/mv-dialogue.toml)。先对至少一个真实非空白姓名
和一个空/纯空白捕获做 Extract，再用未翻译 WriteBack 验证冻结外壳逐字不变。

## 4. Extract Rules

### 4.1 完整根结构和字段表

```toml
[[rule]]
file = 'QuestEntries.json'
path = '[].title'
decode_json = false
pattern = '\A\[title\](?<text>.+)\z'
```

| 字段 | 类型 | 必填/默认 | 约束 |
|---|---|---|---|
| `file` | string | 三类来源选一 | 安全精确 `.json` 基名，或唯一通配 `Map*.json` |
| `plugin` | string | 三类来源选一 | `js/plugins.js` 中精确名称且 `status = true` 的插件 |
| `code` | non-negative integer | 与 `parameter` 成对，三类来源选一 | 扫描 Map、CommonEvents、Troops 的事件命令 |
| `parameter` | non-negative integer | 与 `code` 成对 | 指定参数下标；`parameters` 非数组或缺少该下标使整个候选失败 |
| `path` | string | `file/plugin` 必填；command 可省略 | 非空确定路径，语法见下文 |
| `decode_json` | boolean | 可选，默认 `false` | 要求路径终点 string 再解码一次，结果仍须为 string |
| `pattern` | string | 可选 | 非空 PCRE2；若存在，恰好一个 `text` 命名捕获 |

`file`、`plugin`、`code+parameter` 三种来源恰好选择一个，`code` 与 `parameter` 必须
成对出现。字段到此为止：`label`、`priority`、`required`、`translate` 和版本字段都不存在。

```toml
[[rule]]
file = 'Actors.json'
plugin = 'QuestWindow'
path = 'name'
```

### 4.2 类型、默认值与互斥

三项来源的互斥选择见 4.1。`file/plugin` 必须提供 `path`；command 可省略 `path`，
此时直接读取原始参数：string 进入后续 `decode_json` 和 `pattern`，其余 JSON 类型按类型
聚合为警告并跳过。这个跳过只适用于未提供 `path` 的 command 直接参数；路径、解码和来源
结构仍按各自规则检查。`decode_json` 仅接受 boolean，默认 `false`；`pattern` 省略时使用
整个最终 string，提供时必须是非空 string。
`code`、`parameter` 与固定数组下标都是非负整数，浮点数、数字字符串和负数都不接受。

来源选择是互斥关系而非优先级：字段的排列顺序无法让一个来源盖过另一个。

三类来源的 `path` 都从已经选中的来源值开始，起点固定如下：

| 来源 | `path` 起点 |
|---|---|
| `file` | 文件 JSON 根值 |
| `plugin` | 命中插件的 `parameters` 对象；第一步通常选择参数名 |
| `code + parameter` | 已选中命令的 `parameters[parameter]` 值 |

因此 plugin 规则的 `path = 'entries[].caption'` 先读取名为 `entries` 的插件参数，再按
逐层 JSON 解码规则进入数组；command 规则省略 `path` 时，直接把指定参数作为终点。

### 4.3 解析失败

严格 TOML 形状、来源互斥、字段类型、安全文件名、路径语法、空路径、空正则、PCRE2 编译、
命名捕获形状或未知字段任一不成立，都会在访问来源前拒绝整份文件。quoted key、空对象键
与三层转义分别见[第 4.9 节](#49-路径-ebnf-与-quoted-key)和
[第 2.1 节](#21-pcre2-与三层转义)。

### 4.4 针对来源执行失败

解析成功后，ATT 才对冻结来源执行。执行依次确认来源、参数、路径类型、JSON 解码、最终
字符串和捕获；其中任一必需条件不成立，整个 Rules 候选失败。

路径缺 key 或固定数组越界只让当前展开分支不产出，但每条规则最终都必须至少产出一个
非空单元。command 的扫描跳过、非字符串警告与失败条件见
[第 4.11 节](#411-来源执行与原子失败范围)。

### 4.5 原子失败范围

一份 Extract Rules 文件先整体解析、编译，并针对同一冻结来源执行，再作为一个 Rules
owner 候选提交。任一规则失败、recipe 无法重建或 Mutation Claim 冲突，整个候选都不
提交，旧 Rules owner 快照原样保持；成功时，文件内全部规则的结果一次性替换该 owner，
没有逐规则的部分提交。聚合警告只在匹配、冲突检查和这次提交全部成功后随 Extract 成功
结果返回；它不写入数据库，也不改变后续项目状态。项目日志写入失败仍不改变 Extract
结果。

### 4.6 提供文件、略去参数与显式空数组的生命周期

- 提供非空 `extract --rules FILE`：本次执行整份文件，并原子替换 Rules owner；
- 所有 owner 参数均省略、且上次成功 Extract 方案包含 Rules：直接执行数据库保存的已验证
  canonical 规则语义，不重新读取原 TOML 路径；
- 显式提供其他 owner 而省略 `--rules`：本次不执行 Rules，既有 Rules owner 资产原样
  保持，但 Rules 不进入本次精确替换后的自动方案；
- 提供内容为 `rule = []` 的文件：明确停用并删除 Rules owner 快照，同时把 Rules 移出
  后续自动方案。

省略全部 owner 参数用于复用方案，显式选择其他 owner 用于更换本次方案，空定义用于删除
Rules 资产。项目保存解析后的规范规则内容；原 TOML 移动或删除不影响自动复用。

### 4.7 可复制正例、反例和物化结果

本节 4.1 的 TOML 分别给出最小完整正例和来源互斥反例；4.10 给出多捕获物化后的
Unit 自然顺序与 recipe。可由生产解析器执行并做 WriteBack round-trip 的三来源样例见
[`examples/extract-rules.toml`](examples/extract-rules.toml)。为自己的规则同时准备：一个
实际命中样本、一个形似但不应命中的样本，以及未翻译和已翻译各一次写回结果。

### 4.8 文件身份、Map 和实际大小写

精确文件名必须：

- 是单个 UTF-8 基名，不含控制字符或 `<>:"/\|?*`；
- 以小写 `.json` 结尾，且文件名不只是 `.json`；
- 不是 Windows 设备名 `CON`、`PRN`、`AUX`、`NUL`、`CONIN$`、`CONOUT$`、
  `COM1`～`COM9`、`LPT1`～`LPT9`，也不是 Windows 同样保留的 `COM¹`～`COM³`、
  `LPT¹`～`LPT³`（设备名判断不因额外扩展、尾随空格或点而绕过）；
- 与 `data/` 的实际目录项逐字同大小写。

请求 `items.json` 而实际目录项为 `Items.json` 会明确失败，即使 Windows 能打开前者。
Builtin、Rules 和 WriteBack 使用同一个精确物理身份。

Map 的规范文件名是 `Map` + 1～4,294,967,295 (`u32::MAX`) 的十进制 ID + `.json`：数字
至少三位，且除了三位补零外没有多余前导零。`Map001.json`、`Map999.json`、
`Map1000.json` 是 Map；`Map000.json`、`Map01.json`、`Map0001.json`、
`Map4294967296.json` 不是 Map。这些近似名或越界名若作为精确安全 `file` 提供，则是普通
自定义 DataFile。`Map*.json` 只遍历规范 Map，不匹配它们。

```toml
[[rule]]
file = 'Map000.json'
path = 'displayName'
```

上例合法，但来源身份是自定义 DataFile，不是 Map 0。系统不存在 Map 0。

```toml
[[rule]]
file = 'data/QuestEntries.json'
path = 'title'
```

### 4.9 路径 EBNF 与 quoted key

路径使用下面的确定性语法：

```ebnf
path          = segment, { (".", bare-key) | bracket-step } ;
segment       = bare-key | bracket-step ;
bracket-step  = "[]" | "[", index, "]" | "[", json-string, "]" ;
bare-key      = (ASCII-letter | "_"), { ASCII-letter | ASCII-digit | "_" } ;
index         = ASCII-digit, { ASCII-digit } ;
json-string   = JSON string token, including its double quotes ;
```

`[3]` 是固定零基数组下标，`[]` 按数值顺序展开全部非-null 元素，`["..."]` 是精确对象
键。quoted key 完整采用 JSON string 转义，因此 `path = '[""]'` 合法并选择空对象键；
整个 `path = ''` 仍非法。

```toml
[[rule]]
file = 'QuestEntries.json'
path = '[""]'
```

```toml
[[rule]]
file = 'QuestEntries.json'
path = ''
```

这套语法不支持 JSONPath 的 `$`、递归下降、过滤器、对象键通配或负数下标。包含 Unicode
或标点的对象键使用 `["..."]`，其中的键按 JSON string 转义。

路径继续而当前值为 JSON string 时，ATT 会把该 string 解码成 JSON 后继续；若下一步仍
需要继续且又遇到 string，就再次解码。每一层都记录进 recipe，写回按相反顺序编码。
`decode_json = true` 只在所有路径步骤完成后额外解码一次，并要求额外解码结果仍是
string。

```json
{"payload":"{\"entry\":\"{\\\"title\\\":\\\"星港\\\"}\"}"}
```

```toml
[[rule]]
file = 'QuestEntries.json'
path = 'payload.entry.title'
```

上例在 `payload` 和 `entry` 后各逐层解码一次，最终物化原文 `星港`；不需要
`decode_json = true`。

路径缺 key 或固定数组越界只使当前分支不产出；要求 object/array 却遇到其他 JSON 类型、
自动解码失败、终点不是 string 则使整个规则候选失败。对 command 来源，非 object 命令
项和非整数 `code` 按来源扫描规则跳过；但整数 `code` 已命中后，`parameters` 非数组或
指定的 `parameter` 不存在不是“当前分支跳过”，而是整个候选失败，因为规则声明的事件
协议已经不成立。唯一额外的宽松情况是 command 省略 `path` 后直接拿到非字符串参数；
显式 `path`、字符串 JSON 解码及解码后的终值仍遵守本节严格规则。

### 4.10 整串、局部捕获、顺序与分组

省略 `pattern` 时，最终 string 整体形成一个 Scalar。提供 `pattern` 时：

- 模式不能为空，必须恰好有一个名为 `text` 的命名捕获；
- 完整匹配必须非零宽；`text` 必须参与、非零宽、位于匹配内且落在 UTF-8 字符边界；
- 同一 string 的多次匹配按 `text` 捕获起始字节位置排序且不得重叠；
- 空白 `text` 不产出单元；匹配之外和捕获之外的字节冻结为 Literal；
- 一条非空规则在整个当前来源中至少产出一个非空单元，否则 Rules 候选失败。

被跳过的非字符串不算有效命中。若一条规则只有这类跳过项而没有非空 string 单元，仍按
`rules_no_non_blank_match` 失败；诊断附带各实际类型及数量，便于区分“规则没有目标”和
“来源存在但直接参数类型混合”。候选失败时不提交快照，也不把成功警告交给 CLI。

同一最终 string 内由同一规则的多次捕获自动形成一个组，字段按捕获字节位置排列。需要
提取同一 string 的多个片段时，使用一条规则完成这些捕获。组的自然顺序使用
[第 5 节](#5-语义范围自然顺序与-mutation-claim)定义的 canonical 来源顺序，再按结构路径和捕获
字节位置排列；规则编号不参与排序。数组下标按数值顺序（`2` 在 `10` 前），而不是按
位置字符串排序。

```toml
[[rule]]
file = 'QuestEntries.json'
path = '[].line'
pattern = '<t>(?<text>.*?)</t>'
```

```text
 源值：A<t>第一段</t>B<t>第二段</t>C
物化组：
  第一个 Unit -> "第一段"
  第二个 Unit -> "第二段"
recipe：Literal("A<t>") + Slot(0) + Literal("</t>B<t>") + Slot(1) + Literal("</t>C")
```

尖括号没有隐含提取语义。假设终点完整原值为 `<Help:炎之剑的说明>`：

- 省略 `pattern` 时，完整字符串形成一个 Scalar Unit，recipe 以一个 Slot 写回整个 Value；
- 显式写 `pattern = '\A<Help:(?<text>.*?)>\z'` 时，只有 `炎之剑的说明` 形成 Unit，
  `<Help:` 与 `>` 逐字物化为 recipe Literal；
- 若 Extract 仍省略 `pattern`，但 Translate 的 Custom Placeholder 使用同一个 wrapper
  模式，则 Unit 仍是完整 `<Help:炎之剑的说明>`；Placeholder 只在该 Unit 内保护前后壳，
  不改变 Unit 身份或 recipe。

按需要的翻译边界选择：Extract 捕获让协议壳进入 recipe，Placeholder wrapper 让完整值
保留为 Unit 并在翻译时保护协议壳。`<name:value>` 的外观本身不赋予任何提取语义。

### 4.11 来源执行与原子失败范围

插件来源只读取 `js/plugins.js` 中名称精确匹配且 `status = true` 的参数。插件文件存在、
但插件禁用或名称大小写不同，均不建立该来源。事件来源扫描规范 Map、CommonEvents 和
Troops 中的事件列表；`code` 本身没有 ATT 预设语义。

扫描 command 来源时，非 object 项或 `code` 非整数项按项跳过。整数 `code` 命中规则后，
`parameters` 必须是数组并包含声明的下标，否则整个候选失败。省略 `path` 且直接参数为
非字符串时，ATT 按自然规则号、来源文件、command code、参数下标和实际类型聚合跳过
数量。字符串没有命中 `pattern` 只是普通未命中，不产生警告。

每个命中的事件 command 独立选择自己的一个 `parameters[parameter]`，可选路径和
`pattern` 只作用于这个参数最终得到的单个 string。Rules 不会把相邻 `355/655`、多条
command 或多个参数拼成一个脚本块，也不解析 JavaScript 语法。单条参数内边界确定且可逆
的字面量可以用 Rules；需要跨 command 组合、依赖完整脚本语法或把一个译文同步到多个
目标时，Rules 无法完整表达，应由外部转换建立独立 Generic 项目。

整份 TOML 先严格解析和编译，再针对冻结来源执行。任一规则的来源身份、参数、路径类型、
JSON 解码、最终 string、捕获、重建或物理 Claim 不成立，整个 Rules owner 候选失败，
旧 Rules 快照保持不变。路径缺 key/越界只是该展开分支无产出，但最终仍需满足每条规则
至少产出一个单元。成功时 CLI 的提取摘要仍写 stdout；每类非字符串跳过警告各写一行
stderr，退出码仍为 0。每个聚合警告同时写入 `diagnostic.extract`，只说明规则文件、自然
规则号、命令、参数位置、实际类型、跳过数量和修改方法。警告按上述事实稳定排序，不包含
原始参数值、编码位置或内部状态。

## 5. 语义范围、自然顺序与 Mutation Claim

Builtin 和 Rules 都把来源的物理位置转换成同一种语义顺序。翻译读取器先按语义范围和
自然顺序整理资产，再合并指向同一逻辑位置且 kind 相同的 Group。来源类型不作为
`Builtin → Rules` 排序补充。两个来源对同一 Group 给出不同顺序、同一 Group 出现重复角色，
或者不同对象占用同一顺序时，读取明确失败。

语义范围由来源本身决定：普通数据库文件、System、每张 Map、每个 CommonEvent、每个
Troop 和每个启用插件各自形成范围。TaskBlock 不跨语义范围。范围内的自然顺序来自 JSON
对象成员的插入顺序、数组下标、事件与插件的物理位置；同一物理节点内再用 fragment
区分角色或捕获槽。该顺序不读取 owner、译文状态、Task ID 或任务历史。

Rules 的跨来源顺序是稳定契约，与 OS 目录枚举顺序无关：

| 次序 | Rules 来源 | 来源之间的顺序 |
|---:|---|---|
| 1 | 标准 DataFile | `Actors.json → Animations.json → Armors.json → Classes.json → CommonEvents.json → Enemies.json → Items.json → MapInfos.json → Skills.json → States.json → System.json → Tilesets.json → Troops.json → Weapons.json` |
| 2 | 自定义 DataFile，包括显式选择的非规范近似 Map 名 | 精确 UTF-8 基名字典序 |
| 3 | 规范 Map | MapId 数值升序 |
| 4 | `plugins.js` | 插件数组 index 升序；同一插件内保持 `parameters` 对象成员的来源顺序 |

每个 JSON 来源内部保持对象成员的来源顺序；数组步骤按数值下标；逐层解码后的结构继续按
同一规则；同一最终 string 的多个捕获按捕获起始字节。规则在 TOML 中的编号只用于诊断和
逐条执行，不改变最终顺序。

顺序进入资产快照和完整 Group 语境状态，但不充当对人使用的位置。持久化可以使用内部
顺序键和关联键，CLI、Manual、日志和高级 Lua 只显示源文件、自然编号和字段组成的可读 ID。
并发可以改变完成时间，不能改变语义范围、Group/Unit 自然顺序、稳定装箱或提交顺序。

提取方不直接书写 `MutationClaim`。ATT 从完整 Value 和事件块 recipe 派生资源锁：
`Intent` 表示将穿过资源，`Exclusive` 表示拥有该精确可变 Value。
同一资源只有 `Intent + Intent` 可以共存；存在任一 `Exclusive` 就冲突。验证在组内、
owner 内、跨 owner Store 和 WriteBack 发布前使用同一规则。

完整逻辑 Claim 由 Group kind、来源位置和 recipe 决定。项目数据库只保存 WriteBack 所需的
冲突摘要，WriteBack 会从 recipe 重建完整集合并严格验证，因此持久摘要不会放宽本节任何
冲突规则。Raw Lua 可以直接查看当前表结构；普通工作流不要求使用这些内部关联定位对象。

| 两个声明的关系 | 结果 |
|---|---|
| 同一 raw JSON Value | 冲突 |
| raw JSON string 与它解码后的任意 descendant | 冲突 |
| 同一已解码对象中的不同 sibling | 允许 |
| Dialogue/Choices/ScrollingText 事件块与其覆盖字段或 decoded descendant | 冲突 |
| 两个互不覆盖的事件块或普通值 | 允许 |

冲突取决于两个 recipe 是否竞争同一物理资源，与最终字符串是否相等或是否包含 `<`、`>`
无关。

## 6. RPG Maker Placeholder Rules

本节建立在[公共 Placeholder 规格](../translation/placeholders.md)之上。严格 TOML、
`pattern`、可选 `scopes` 与 `ids`、`order`、`text` 捕获和 token 恢复都沿用那里的解释；下面只规定 MV/MZ
能使用的 scope、Builtin 控制符与数组槽行为。

### 6.1 RPG Maker scope

```toml
[[rule]]
scopes = ['event_dialogue', 'event_choices']
order = 'preserve'
pattern = '\\SE\[[^]]+\]'

[[rule]]
scopes = ['plugin_parameter']
ids = ['plugins.js:QuestWindow:Title']
order = 'preserve'
pattern = '<name>(?<text>.*?)</name>'
```

合法 scope 共八个：`database_entry`、`system`、`map`、`event_dialogue`、`event_choices`、
`event_scrolling_text`、`event_command`、`plugin_parameter`；`all`、别名和父 scope 都不
存在。

无 `text` 捕获时，完整匹配整段都是不透明保护区；有 `text` 时，捕获前后的 wrapper 是
不透明边界，`text` 捕获本身是继续交给模型翻译的 NaturalText。

### 6.2 类型、默认值与互斥

字段类型、必填项和捕获约束见[公共 Placeholder 规格](../translation/placeholders.md#文件字段与匹配)。
RPG Maker 的 `scopes` 省略时适用于全部八个 kind；提供时只接受本节列出的值。`ids` 选择
当前项目的完整自然 ID，与 `scopes` 取交集。规则是否声明 `text` 决定投影方式，所有实际
匹配都必须遵守该方式。

### 6.3 解析失败

根/字段/类型错误、空模式、无效 PCRE2、非法命名捕获、空/重复/未知 scope 或 ID，以及
wrapper 使用 `reorder_within_slot`，都会在读取
资源时拒绝整份自定义 Placeholder 定义。Builtin 控制符不来自该文件，因此清空自定义
定义不会关闭 Builtin。PCRE2 及 TOML 转义见[第 2.1 节](#21-pcre2-与三层转义)。

### 6.4 针对来源执行失败

定义成功后，规则只对 kind 与 `scopes`、自然 ID 与 `ids` 都相符的 Unit 执行。kind 来自
Builtin 或 Rules；owner、文件路径和自然规则号不参与 scope 选择。单条自定义规则零命中
是正常结果；实际匹配、捕获与保护跨度的要求见第 6.9 节。

保护跨度冲突、跨越 `Lines` 元素槽边界、原文占用保留前缀 `⟦ATT_`，或 token 无法安全投影
时，诊断定位到当前 Unit，整次 Translate 规划失败。ATT 不删去失败 Unit，也不发送任何
其他模型任务；数据库保持不变。

### 6.5 原子失败范围

文件解析或编译失败时，项目原有自定义 Placeholder 资源保持不变。针对来源的准备错误同样
终止整次规划，结果为 Failed，并在任何模型请求之前报告规则文件、自然规则号、Unit 位置、
原因和修改方法。规划失败保留数据库原状。规划完成后的逐 ID 候选拒绝属于另一阶段，按
[翻译规格](translation.md)保留合法响应；规则文件没有忽略冲突或优先级开关。

### 6.6 提供文件、略去参数与显式空数组的生命周期

- 提供非空 `--placeholders FILE`：完整替换项目自定义 Placeholder 资源；
- 省略 `--placeholders`：复用项目当前资源，不重新解析外部文件；
- 提供内容为 `rule = []` 的文件：清空自定义规则，但 Builtin 保护继续生效。

候选解析或编译失败时，项目中原有的自定义资源保持不变。

### 6.7 可复制正例、反例和物化结果

本节 6.1 的 TOML 是可复制正例；将其中的 `pattern` 改成 `''` 可以验证空模式的编译失败。
完整多 scope 文件见
[`examples/placeholders.toml`](examples/placeholders.toml)。第 6.9 节给出 wrapper、
NaturalText 与 Builtin 控制符共同物化的结果；测试时应同时覆盖命中 scope、不命中 scope、
opaque 壳和壳内正文。

### 6.8 内建控制符

MV/MZ 的标准控制语法和标准字段消费者是 ATT 的默认领域事实。开发时用于确认这些事实的
官方 core、历史实现和真实样本不进入生产流程；项目运行时不扫描 core、不计算函数摘要，
也不因活动插件覆写重新决定内建规则。实现根据项目引擎与 Unit 的标准物理来源直接应用下表。
Rules 命中标准事件参数时，Extract 同时冻结实际命令编号，后续按物理参数位置和命令编号
继承内建控制规则；插件参数或嵌套解码后的字段不因外形相似而获得标准字段的消费者身份。

下表的每种语法都依赖最后一列所列的标准字段消费者：

| 语法族 | MV | MZ | 只有何种消费者成立时才保护 |
|---|:---:|:---:|---|
| `%[0-9]+` | 有 | 有 | 标准字段是 `String.prototype.format` 的格式字符串 |
| `\\`、`\V[n]`、`\N[n]`、`\P[n]`、`\G` | 有 | 有 | 标准字段由扩展文字消费者处理 |
| `\C[n]`、`\I[n]`、`\{`、`\}` | 有 | 有 | 标准字段由扩展文字消费者处理 |
| `\PX[n]`、`\PY[n]`、`\FS[n]` | 无 | 有 | 标准 MZ 字段由扩展文字消费者处理 |
| `\$`、`\.`、`\|`、`\!`、`\>`、`\<`、`\^` | 有 | 有 | 标准字段由 Message 消费者处理 |
| U+000C | 有 | 有 | 标准字段由 Message 消费者处理 |

`n` 只由 ASCII `[0-9]` 组成，命令名只按 MV/MZ 的 ASCII 大小写规则解释。插件新增控制语法
或把某种语法用于非标准字段时，该外形只进入 Review；明确确认后使用 Custom Placeholder，
不能把插件事实扩成所有 MV/MZ 项目的内建规则。

反斜杠与 U+001B 按引擎转换顺序成对转义；每对表示一个普通反斜杠，并作为一段扩展控制符
保护。连续序列中剩余的单个反斜杠或 U+001B 才能引出后续控制命令。因此，`\\.` 中的句点
是普通正文，`\\\.` 的最后一个反斜杠与句点共同形成等待控制符，`\\\\.` 则只保护两对反斜杠。

同一槽内的 `%N` 可以按目标语言语序重排；身份、数量和槽位必须保持。其他内建控制符保持
相对顺序。两类规则都不允许增加、删除或移动到另一槽。

`\N<...>`、`\N1<...>`、`\NC<...>` 等 inline 姓名框不是 MV/MZ 通用内建语法。MZ 原生
speaker 是事件命令 101 的独立字段；MV 没有原生姓名框。当前游戏确实使用 inline wrapper
时，使用带精确 `ids` 和 `text` 捕获的 Custom Placeholder Rule 保护前后壳，姓名正文仍是
NaturalText。裸 `<`、`>` 以及没有消费者证据的反斜杠形式同样只进入 Review。
排版层也不会因它们“看起来像控制符”就当作零宽字符；只有上游语义所有者已建立的 ATT
Placeholder token 按受保护内容处理，其他反斜杠形式按可见文本计量。

### 6.9 匹配、NaturalText 与重叠

完整匹配必须非零宽并落在 UTF-8 字符边界；`text` 若存在，必须参与、位于匹配内并落在
UTF-8 字符边界。自定义规则零命中合法。ATT 先求出每条规则实际保护的跨度，再检查冲突：
保护跨度相交才冲突。wrapper 的 NaturalText 捕获中可以继续出现 Builtin 控制符，Builtin
会在 NaturalText 内部保护它，两个完整正则范围互相包含也不会误判。

`Value` 是一个完整标量，规则可以按 PCRE2 设置跨其中的 LF 匹配。`Lines` 则保留元素槽
边界：无 `text` 捕获的完整 opaque 匹配不得跨元素；有 `text` 捕获时，完整 wrapper 匹配
可以跨元素，但 `<msg>`、`</msg>` 这类实际 opaque 前后壳必须各自位于单个元素，拼接 LF
只能留在 NaturalText 捕获中。

```toml
[[rule]]
scopes = ['event_dialogue']
order = 'preserve'
pattern = '<msg>(?<text>.*?)</msg>'
```

对于 `<msg>勇者\C[2]</msg>`，`<msg>`/`</msg>` 是自定义 opaque wrapper，`勇者` 是
NaturalText，`\C[2]` 是 NaturalText 内的 Builtin 保护段。三者可以自然组合。

同理，Extract 省略 `pattern` 得到的完整 Unit `<Help:炎之剑的说明>`，可以在
`database_entry` scope 使用 `\A<Help:(?<text>.*?)>\z`：前后壳成为 opaque，正文保持
NaturalText。Unit 原文、Group、recipe 和持久身份保持不变；保护后的文本及实际 binding
参与本次去重。候选验收会检查 wrapper 的原配对、捕获形状和拓扑，保证已确认的前后壳。

如果目标是让 `<Help:` 与 `>` 从提取时就固定在写回结构中，则在 Extract Rule 中使用
`text` 捕获正文，并重新 Extract，使前后壳进入 recipe。Placeholder 负责当前 Unit 内已确认
片段的保护，完整来源 grammar 与写回关系仍由 Extract 表达；只有后者超出 Rules 能力时，
才交由外部转换和独立 Generic 项目处理。

例如源 `Lines` 为 `["<msg>第一行", "第二行</msg>"]` 时，若模式启用 DOTALL，元素边界
位于 `text` 捕获中，因此合法；无 `text` 捕获并把两行整体保护的模式则使该单元规划失败。

Custom/Custom 或 Custom/Builtin 的实际保护跨度相交时，诊断指出本 Unit 的冲突，整次规划
失败。规则顺序不会赋予优先级，也不会让最长匹配覆盖另一条规则。

### 6.10 token、FullyProtected 与 Current

前缀 `⟦ATT_` 属于 ATT token 命名空间，原文占用它会使规划失败。token 根据实际保护类别、
segment 和源位置顺序生成。自定义规则编号只用于诊断；插入、删除或重排一条未命中当前
Unit 的规则，不改变它的保护后模型文本。

若去掉全部 opaque 段后没有任何非空白 NaturalText，prepared 状态为
`fully_protected`，不请求 LLM；只剩空格、制表符或换行也属于这种情况。
候选验收使用源文已建立的 binding。候选保留 token 时直接核对；回显原始片段时，按完整
数量、自然顺序和允许槽位绑定到源 token。多个同字节片段只要仍能唯一归属就可以绑定；
数量不符、重叠或无法唯一归属时拒绝。`fixed` 按槽分别绑定，允许换序也只限于同一槽。

预期片段绑定后，ATT 扫描完整候选，拒绝源 binding 之外新增的 Custom 或内建控制身份。
规则无需在译文的自然语言上下文再次命中既有片段；wrapper 仍须保持配对、捕获形状和拓扑。
模型响应、Manual、受管 Lua、Current 复验和 WriteBack 共用
[公共 Placeholder 验收](../translation/placeholders.md#候选验收)。

Placeholder binding 不进入自动译文的适用性指纹。当前原文、完整 Group 语境、语言对、
位置、角色与写回结构先决定正文是否适用，当前 Placeholder 强契约再独立复验。规则改变后，
仍合法的正文继续为 Current；违反当前强不变量的正文保留并转入 Rejected。规则诊断编号、
并发和重试变化不会使正文失效。

## 7. Terminology 与协议壳的边界

术语只在 Placeholder 投影后的每段 NaturalText 内逐段匹配，不扫描 opaque 协议壳，也
不跨两个 `OpaqueBoundary` 拼接。模型 Prompt 复用同一次有序命中结果；术语不参与既有译文的
Current 身份。

假设术语 trigger 是 `勇者`：

```text
原文：<actor title="勇者">勇者</actor>
规则：<actor title="勇者">(?<text>.*?)</actor>

opaque 前壳中的“勇者”：不命中
NaturalText 捕获中的“勇者”：命中
opaque 后壳：不扫描
```

即使 trigger 的前半在一个 NaturalText 段、后半在另一个 NaturalText 段，它同样不命中。
术语文件完整契约见[公共术语规格](../translation/terminology.md)。

## 8. 用当前来源验证规则

加载到正式项目之前，使用当前游戏的命中样本和不应命中的样本验证：

1. 来源是实际启用的消费者读取的精确物理身份，文件大小写与目录项一致；
2. 正例能命中，形似的代码、资源名、禁用插件或不可达内容不命中；
3. 路径每一层的 JSON 类型和逐层解码都有真实样本；
4. 完整匹配、命名捕获、Literal 和最终写回边界符合协议；
5. group/unit 自然顺序与真实显示顺序一致；
6. Mutation Claim 没有与 Builtin 或另一规则竞争；
7. 未翻译 round-trip 逐字保持，翻译 round-trip 只改变允许的槽；
8. 重复 Extract/Translate 收敛，未命中资源变化不制造无关重译。

完整阶段事务见[提取规格](extraction.md)，译文状态与验收见[翻译规格](translation.md)，
物化 recipe 的写回见[写回规格](write-back.md)。
