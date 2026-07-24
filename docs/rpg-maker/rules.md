# RPG Maker 规则文件现行规格与编写指南

本文是 RPG Maker MV/MZ 三类声明式规则文件的唯一现行规范：

- MV 对话姓名投影（`--dialogue-rules`）；
- Extract Rules（`extract --rules`）；
- Placeholder Rules（`translate --placeholders`）。

字段、默认值、互斥关系、解析失败和执行失败均以本文为准。提取、翻译阶段文档只说明
生命周期、事务与状态交接，不重复定义字段。如果代码或测试与本文冲突，应修正实现或
本文，使系统重新只有一套契约；外部作者不需要阅读源码来猜测真实行为。

本文所有规范代码块前都有机器可读标记：

- `<!-- att-example: valid -->`：可直接交给相应生产解析器的完整有效输入；
- `<!-- att-example: invalid -->`：必须被生产边界拒绝的完整输入；
- `<!-- att-example: illustrative -->`：语法片段、数据夹具或物化结果，不是完整规则文件。

## 1. 三类文件解决不同问题

| 文件 | 命令位置 | 它拥有的事实 | 它不负责 |
|---|---|---|---|
| MV 姓名投影 | `mv extract --builtin --dialogue-rules FILE` | 标准 MV 对话第一条 `401` 中的 Speaker 投影 | 插件消息、任意文本提取、控制符保护 |
| Extract Rules | `mv|mz extract --rules FILE` | 已知来源、确定路径、最终字符串及可逆写回边界 | 猜测可见性、跨文档关系、多目标同步 |
| Placeholder Rules | `mv|mz translate ... --placeholders FILE` | 已提取文本中不可让模型改写的协议跨度 | 新增提取位置、修复错误分组 |

真实关系需要动态键枚举、条件筛选、跨文档判断、一个译文写到多个目标或脚本私有状态
时，使用 [Lua 技术参考](lua.md)和 [Lua Cookbook](lua-cookbook.md)。Lua 自由度很高；
停止线来自协议所有权，而不是脚本能力不足。

## 2. 共同根结构、严格解析与生命周期

三类文件都是严格 UTF-8 TOML，根恰好包含 `rule` 数组。非空定义使用一个或多个
`[[rule]]`；权威空定义统一写作：

<!-- att-example: valid -->
```toml
rule = []
```

零字节文件、只有注释的文件、缺少 `rule`、未知字段、重复字段或错误类型都属于普通
无效输入，不等于空定义。文件只接受本规格列出的根、字段和值。

<!-- att-example: invalid -->
```toml
# 缺少必需的 rule 根；注释文件不是空定义。
```

文件参数的生命周期如下：

| 输入方式 | MV 姓名投影 | Extract Rules | Placeholder Rules |
|---|---|---|---|
| 提供非空文件 | 完整替换项目姓名定义，并与本次 Builtin 一起执行 | 完整执行并原子替换 Rules owner | 完整替换自定义占位符资源 |
| 提供 `rule = []` | 清空姓名定义 | 停用并删除 Rules owner 快照 | 清空自定义规则；Builtin 保护仍在 |
| 省略参数 | 复用项目当前定义，不重新解析文件 | 所有 owner 参数均省略时按上次成功方案复用；显式选择其他 owner 时不执行且既有资产不变 | 复用项目当前资源 |

`--dialogue-rules` 只允许与 MV `--builtin` 同时使用。三个文件任一候选失败，都不会用
半成品覆盖它对应的旧状态。阶段级的先后提交语义见[提取规格](extraction.md)和
[翻译规格](translation.md)。

### 2.1 PCRE2 与三层转义

三类规则的正则均使用 PCRE2，开启 UTF 与 UCP。它不是 JavaScript `RegExp`：不要使用
JavaScript 正则字面量 `/.../flags`；标志应写在模式内（如 `(?i)`），Unicode 属性、
命名捕获、锚点和替换语义以 PCRE2 为准。`.` 默认不匹配 LF；确实需要让单次匹配跨越
多行时，必须在模式中显式启用 DOTALL，例如 `(?s)`。DOTALL 只决定 PCRE2 是否能看到
LF，不会取消 Placeholder 的 `Lines` 槽边界：匹配可以跨 `Value` 内部 LF，但实际 opaque
保护跨度不得吞入两个 `Lines` 元素之间的拼接 LF。

推荐使用 TOML 单引号字面字符串，让反斜杠原样传给 PCRE2：

本小节下面两个标为 `valid` 的完整 `[[rule]]` 采用 Placeholder Rules 的字段形状，并由
Placeholder 生产解析与编译边界验收；MV 姓名规则和 Extract Rules 使用相同的 TOML/PCRE2
转义原则，但还必须分别补齐 `speaker` 捕获或 Extract 来源与 `text` 捕获。

<!-- att-example: valid -->
```toml
[[rule]]
pattern = '\A(?<text>正文)\z'
```

写 Extract 路径中的 quoted key 时，最多同时存在三层语法，必须逐层计算：

1. TOML 字符串；
2. 路径中 quoted key 使用的 JSON string；
3. `pattern` 才有 PCRE2 转义。

例如，键的实际字节是 `a"b`（字母、双引号、字母）时，使用 TOML 单引号保住路径文本，
再按 JSON string 把双引号写成 `\"`：

<!-- att-example: illustrative -->
```toml
path = '["a\"b"]'
```

匹配游戏文本中的字面 `\SE[Bell]`，PCRE2 仍需用 `\\` 匹配一个反斜杠：

<!-- att-example: valid -->
```toml
[[rule]]
pattern = '\\SE\[[^]]+\]'
```

## 3. MV 对话姓名投影

### 3.1 完整根结构与字段

每项只有一个字段：

| 字段 | 类型 | 必填 | 默认值 | 约束 |
|---|---|---:|---|---|
| `pattern` | string | 是 | 无 | 非空 PCRE2；必须恰好有一个名为 `speaker` 的命名捕获 |

<!-- att-example: valid -->
```toml
[[rule]]
pattern = '\A\\N<(?<speaker>[^>]*)>\z'
```

未命名捕获可以使用；除 `speaker` 外的任何命名捕获都非法。`pattern = ""` 在编译边界
作为空模式错误拒绝，而不是被当成一个到处零宽命中的规则。

<!-- att-example: invalid -->
```toml
[[rule]]
pattern = ''
```

### 3.2 类型、默认值与互斥

`pattern` 只能是 TOML string，没有默认值。每项只能出现这一个字段；缺少它、写成数组、
同时写入 Extract/Placeholder 字段或出现未知字段，都会使整份文件无效。每个模式必须且
只能声明一个 `speaker` 命名捕获；未命名捕获允许存在，其他命名捕获不允许存在。

姓名定义只适用于 MV Builtin；MZ 的原生 Speaker 不与此规则互斥，而是完全不消费此文件。

### 3.3 解析失败

下列问题在读取或编译候选时拒绝整份定义，不接触冻结游戏来源：根不是 `rule` 数组、字段
缺失/重复/未知、类型错误、空模式、无效 PCRE2、`speaker` 数量不为一，或存在其他命名
捕获。PCRE2 与转义边界以[第 2.1 节](#21-pcre2-与三层转义)为准。

### 3.4 针对来源执行失败

姓名投影只扫描标准 MV 事件消息块 `101 + 连续 401*` 的第一条
`401.parameters[0]`。MZ 使用 `101.parameters[4]` 的原生 Speaker，不读取此文件。

每个完整匹配必须非零宽、位于 UTF-8 字符边界；`speaker` 必须参与并完全位于该匹配内。
`speaker` 捕获本身允许空字符串或纯空白。ATT 仅用 `speaker.trim().is_empty()` 判断是否
建立 Speaker；一旦建立，捕获的原始字节（包括首尾空白）原样保存、比较和写回，不 trim、
不规范化。单次空/纯空白捕获是合法命中；但一条非空规则在整份当前冻结来源执行完后，
仍必须至少建立过一个非空白 Speaker，证明这条规则在本游戏中有现实消费。

一条规则可在同一第一行产生多个不重叠匹配。所有实际建立的 Speaker 必须原始字节完全
相同。两条不同规则只要都命中同一第一条 `401` 就冲突，不要求两个跨度重叠，也没有
“先写优先”。规则序号只用于错误定位。

ATT 将从行首到最后一个 marker 结束处投影成 Literal/SpeakerSlot。最后 marker 后：

- 含任一非空白字符：整个后缀成为该行 Body；
- 为空或纯空白：后缀冻结成 Literal，不建立 Body。

后续连续 `401` 仍组成同一 DialogueBody。空或纯空白 `speaker` 只冻结已匹配外壳，不建立
Speaker。

<!-- att-example: illustrative -->
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
- 投影与 Builtin/Rules/Lua 的物理修改声明冲突。

规则不应依靠“像姓名”猜测整行。应使用当前游戏消费协议的 marker、锚点和反例验证。

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

<!-- att-example: valid -->
```toml
[[rule]]
file = "QuestEntries.json"
path = '[].title'
decode_json = false
pattern = '\A\[title\](?<text>.+)\z'
```

| 字段 | 类型 | 必填/默认 | 约束 |
|---|---|---|---|
| `file` | string | 三类来源选一 | 安全精确 `.json` 基名，或唯一通配 `Map*.json` |
| `plugin` | string | 三类来源选一 | `js/plugins.js` 中精确名称且 `status = true` 的插件 |
| `code` | non-negative integer | 与 `parameter` 成对，三类来源选一 | 扫描 Map、CommonEvents、Troops 的事件命令 |
| `parameter` | non-negative integer | 与 `code` 成对 | 指定参数下标；缺少该参数使整个规则候选失败 |
| `path` | string | `file/plugin` 必填；command 可省略 | 非空确定路径，语法见下文 |
| `decode_json` | boolean | 可选，默认 `false` | 要求路径终点 string 再解码一次，结果仍须为 string |
| `pattern` | string | 可选 | 非空 PCRE2；若存在，恰好一个 `text` 命名捕获 |

`file`、`plugin`、`code+parameter` 恰好选择一个来源。`parameter` 不能单独出现，`code`
也不能省略 `parameter`。没有 `label`、`priority`、`required`、`translate` 或版本字段。

<!-- att-example: invalid -->
```toml
[[rule]]
file = "Actors.json"
plugin = "QuestWindow"
path = 'name'
```

### 4.2 类型、默认值与互斥

每项恰好选择 `file`、`plugin`、`code+parameter` 三种来源之一。`file/plugin` 必须提供
`path`；command 可省略 `path`，此时参数自身必须是最终 string。`decode_json` 仅接受
boolean，默认 `false`；`pattern` 省略时使用整个最终 string，提供时必须是非空 string。
`code`、`parameter` 与固定数组下标都是非负整数，不接受浮点数、数字字符串或负数。

来源选择是互斥关系，不是优先级；不能靠字段排列让其中一个来源覆盖另一个来源。

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

解析成功后，ATT 才对冻结来源执行。精确文件/插件/事件来源不存在，实际大小写不符，插件
未启用，command 缺少声明的 `parameter`，路径要求的 JSON 类型不符，逐层解码失败，终点
不是 string，捕获非法，或规则最终未产出任何非空单元，都会使候选失败。路径缺 key 或
固定数组越界只让当前展开分支不产出；它不是候选失败，除非最终导致该规则零产出。

完整来源行为见[第 4.11 节](#411-来源执行与原子失败范围)。

### 4.5 原子失败范围

一份 Extract Rules 文件先整体解析、编译并针对同一冻结来源执行，再作为一个 Rules owner
候选提交。任一规则失败、recipe 无法重建或 Mutation Claim 冲突，整个候选不提交；旧
Rules owner 快照保持不变。成功时文件内全部规则的结果一次性替换该 owner，不做逐规则
部分提交。

### 4.6 提供文件、略去参数与显式空数组的生命周期

- 提供非空 `extract --rules FILE`：本次执行整份文件，并原子替换 Rules owner；
- 所有 owner 参数均省略、且上次成功 Extract 方案包含 Rules：直接执行数据库保存的已验证
  canonical 规则语义，不重新读取原 TOML 路径；
- 显式提供其他 owner 而省略 `--rules`：本次不执行 Rules，既有 Rules owner 资产原样
  保持，但 Rules 不进入本次精确替换后的自动方案；
- 提供内容为 `rule = []` 的文件：明确停用并删除 Rules owner 快照，同时把 Rules 移出
  后续自动方案。

因此“整组 owner 参数省略”“显式选择其他 owner 时未列出 Rules”和“传空定义”是三个
不同意图，不能互换。保存方案持有 canonical 语义而不是文件路径，所以原 TOML 移动或
删除不影响自动复用。

### 4.7 可复制正例、反例和物化结果

本节 4.1 的 TOML 分别给出最小完整正例和来源互斥反例；4.10 给出多捕获物化后的
`unit_order` 与 recipe。可由生产解析器执行并做 WriteBack round-trip 的三来源样例见
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
Builtin、Rules、Lua `data_file` 和 WriteBack 使用同一个精确物理身份。

Map 的规范文件名是 `Map` + 1～4,294,967,295 (`u32::MAX`) 的十进制 ID + `.json`：数字
至少三位，且除了三位补零外没有多余前导零。`Map001.json`、`Map999.json`、
`Map1000.json` 是 Map；`Map000.json`、`Map01.json`、`Map0001.json`、
`Map4294967296.json` 不是 Map。这些近似名或越界名若作为精确安全 `file` 提供，则是普通
自定义 DataFile。`Map*.json` 只遍历规范 Map，不匹配它们。

<!-- att-example: valid -->
```toml
[[rule]]
file = "Map000.json"
path = 'displayName'
```

上例合法，但来源身份是自定义 DataFile，不是 Map 0。系统不存在 Map 0。

<!-- att-example: invalid -->
```toml
[[rule]]
file = "data/QuestEntries.json"
path = 'title'
```

### 4.9 路径 EBNF 与 quoted key

路径不是 JSONPath。完整语法为：

<!-- att-example: illustrative -->
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

<!-- att-example: valid -->
```toml
[[rule]]
file = "QuestEntries.json"
path = '[""]'
```

<!-- att-example: invalid -->
```toml
[[rule]]
file = "QuestEntries.json"
path = ''
```

不支持 `$`、递归下降、过滤器、对象键通配、负数下标或方括号外的任意 Unicode key。

路径继续而当前值为 JSON string 时，ATT 会把该 string 解码成 JSON 后继续；若下一步仍
需要继续且又遇到 string，就再次解码。每一层都记录进 recipe，写回按相反顺序编码。
`decode_json = true` 只在所有路径步骤完成后额外解码一次，并要求额外解码结果仍是
string。

<!-- att-example: illustrative -->
```json
{"payload":"{\"entry\":\"{\\\"title\\\":\\\"星港\\\"}\"}"}
```

<!-- att-example: valid -->
```toml
[[rule]]
file = "QuestEntries.json"
path = 'payload.entry.title'
```

上例在 `payload` 和 `entry` 后各逐层解码一次，最终物化原文 `星港`；不需要
`decode_json = true`。

路径缺 key 或固定数组越界只使当前分支不产出；要求 object/array 却遇到其他 JSON 类型、
自动解码失败、终点不是 string 则使整个规则候选失败。对 command 来源，指定的
`parameter` 不存在不是“当前分支跳过”，而是整个候选失败，因为其声明的事件协议已经
不成立。

### 4.10 整串、局部捕获、顺序与分组

省略 `pattern` 时，最终 string 整体形成一个 Scalar。提供 `pattern` 时：

- 模式不能为空，必须恰好有一个名为 `text` 的命名捕获；
- 完整匹配必须非零宽；`text` 必须参与、非零宽、位于匹配内且对齐 UTF-8；
- 同一 string 的多次匹配按 `text` 捕获起始字节位置排序且不得重叠；
- 空白 `text` 不产出单元；匹配之外和捕获之外的字节冻结为 Literal；
- 一条非空规则在整个当前来源中至少产出一个非空单元，否则 Rules 候选失败。

同一最终 string 内由同一规则的多次捕获自动形成一个组，字段按捕获字节位置排列；不要
用多条规则瓜分同一个 string。组的自然顺序使用[第 5 节](#5-自然顺序与-mutation-claim)
定义的 canonical 来源顺序，再按结构路径和捕获字节位置排列；规则编号不参与排序。数组
下标按数值顺序（`2` 在 `10` 前），不是按位置字符串排序。

<!-- att-example: valid -->
```toml
[[rule]]
file = "QuestEntries.json"
path = '[].line'
pattern = '<t>(?<text>.*?)</t>'
```

<!-- att-example: illustrative -->
```text
源值：A<t>第一段</t>B<t>第二段</t>C
物化组：
  unit_order 0 -> "第一段"
  unit_order 1 -> "第二段"
recipe：Literal("A<t>") + Slot(0) + Literal("</t>B<t>") + Slot(1) + Literal("</t>C")
```

### 4.11 来源执行与原子失败范围

插件来源只读取 `js/plugins.js` 中名称精确匹配且 `status = true` 的参数。插件文件存在、
但插件禁用或名称大小写不同，均不建立该来源。事件来源扫描规范 Map、CommonEvents 和
Troops 中的事件列表；`code` 本身没有 ATT 预设语义。

整份 TOML 先严格解析和编译，再针对冻结来源执行。任一规则的来源身份、参数、路径类型、
JSON 解码、最终 string、捕获、重建或物理 Claim 不成立，整个 Rules owner 候选失败，
旧 Rules 快照保持不变。路径缺 key/越界只是该展开分支无产出，但最终仍需满足每条规则
至少产出一个单元。

## 5. 自然顺序与 Mutation Claim

最终标准资产的 owner 总顺序固定为 `Builtin → Rules → Lua`。每个 owner 的 `group_order`
从 0 连续，每组 `unit_order` 从 0 连续：

- Builtin：来源结构顺序、本文声明的字段顺序、数组数值顺序；
- Rules：下表定义的 canonical 来源顺序、来源内部结构路径顺序、同一 string 的捕获字节
  位置；
- Lua：脚本提交的 `groups` 数组和每组 `fields` 数组声明顺序。

Rules 的跨来源顺序是稳定契约，不读取也不继承 OS 目录枚举顺序：

| 次序 | Rules 来源 | 来源之间的顺序 |
|---:|---|---|
| 1 | 标准 DataFile | `Actors.json → Animations.json → Armors.json → Classes.json → CommonEvents.json → Enemies.json → Items.json → MapInfos.json → Skills.json → States.json → System.json → Tilesets.json → Troops.json → Weapons.json` |
| 2 | 自定义 DataFile，包括显式选择的非规范近似 Map 名 | 精确 UTF-8 基名字典序 |
| 3 | 规范 Map | MapId 数值升序 |
| 4 | `plugins.js` | 插件数组 index 升序；同一插件内保持 `parameters` 对象成员的来源顺序 |

每个 JSON 来源内部保持对象成员的来源顺序；数组步骤按数值下标；逐层解码后的结构继续按
同一规则；同一最终 string 的多个捕获按捕获起始字节。规则在 TOML 中的编号只用于诊断和
逐条执行，不改变最终顺序。

顺序进入资产快照指纹，但不进入逻辑身份，也不单独阻止原文/上下文相同的译文继承。
并发可以改变完成时间，不能改变这些顺序。

提取方不直接书写 `MutationClaim`。ATT 从 Value、NoteTag、CommentTag 和事件块 recipe
派生资源锁：`Intent` 表示将穿过或局部使用资源，`Exclusive` 表示拥有该精确可变资源。
同一资源只有 `Intent + Intent` 可以共存；存在任一 `Exclusive` 就冲突。验证在组内、
owner 内、跨 owner Store 和 WriteBack 发布前使用同一规则。

完整逻辑 Claim 由 group kind、location 和 recipe 决定并进入 owner 指纹。项目表
`standard_mutation_claim` 不是这份完整清单，而是跨 owner 冲突摘要：每个
`(owner, resource)` 至多一行；唯一 Exclusive 原样保留，共享 resource 的多个合法 Intent
只保留自然顺序最早的 group 代表。WriteBack 会从 recipe 重建完整集合并严格验证该摘要，
因此摘要不会放宽本节任何冲突规则。

| 两个声明的关系 | 结果 |
|---|---|
| 同一 raw JSON Value | 冲突 |
| raw JSON string 与它解码后的任意 descendant | 冲突 |
| 同一已解码对象中的不同 sibling | 允许 |
| raw `note` 与其中任一 NoteTag | 冲突 |
| 同一 NoteTag occurrence | 冲突 |
| 同一 `note` 的不同 tag occurrence | 允许 |
| raw `108/408` comment string 与其 CommentTag | 冲突 |
| 同一 CommentTag occurrence | 冲突 |
| 同一 comment 块的不同 tag occurrence | 允许 |
| Dialogue/Choices/ScrollingText 事件块与其覆盖字段或 decoded descendant | 冲突 |
| 两个互不覆盖的事件块或普通值 | 允许 |

因此“最终字符串文字刚好相等”不是冲突判断；关键是两个 recipe 是否竞争同一物理资源。

## 6. Placeholder Rules

### 6.1 完整根结构、字段和 scope

<!-- att-example: valid -->
```toml
[[rule]]
scopes = ["event_dialogue", "event_choices"]
pattern = '\\SE\[[^]]+\]'

[[rule]]
scopes = ["plugin_parameter"]
pattern = '<name>(?<text>.*?)</name>'
```

| 字段 | 类型 | 必填/默认 | 约束 |
|---|---|---|---|
| `pattern` | string | 必填 | 非空 PCRE2；无命名捕获，或恰好一个 `text` |
| `scopes` | string array | 可选；省略表示全部 scope | 显式数组必须非空、无重复、只含下表值 |

合法 scope：`database_entry`、`system`、`map`、`event_dialogue`、`event_choices`、
`event_scrolling_text`、`event_command`、`plugin_parameter`。没有 `all`、别名或父 scope。

无 `text` 捕获时完整匹配是不透明保护段；有 `text` 时，完整匹配中捕获前后的 wrapper
是不透明边界，捕获本身仍是 NaturalText。`text` 不是“要保护的内容”。

### 6.2 类型、默认值与互斥

`pattern` 是必填的非空 TOML string。`scopes` 省略表示全部八个精确 scope；显式提供时
必须是非空 string array，不能重复，也不能写 `all`。模式要么没有命名捕获，要么恰好
只有一个 `text`；其他命名捕获不允许。无 `text` 与有 `text` 是两种互斥投影形态，不会
按匹配结果自动切换。

### 6.3 解析失败

根/字段/类型错误、空模式、无效 PCRE2、非法命名捕获、空/重复/未知 scope，都会在读取
资源时拒绝整份自定义 Placeholder 定义。Builtin 控制符不来自该文件，因此清空自定义
定义不会关闭 Builtin。PCRE2 及 TOML 转义见[第 2.1 节](#21-pcre2-与三层转义)。

### 6.4 针对来源执行失败

定义成功后，规则只对 scope 相符的标准单元执行。单条自定义规则零命中是正常结果；一旦
命中，完整匹配必须非零宽并位于 UTF-8 边界，`text` 必须参与、位于完整匹配内并对齐。
实际保护跨度冲突、跨越 `Lines` 元素的语义槽边界、原文占用保留前缀 `⟦ATT_`，或最终
token 无法安全投影时，只使当前翻译单元规划失败，不把未命中的其他单元判为失败。

### 6.5 原子失败范围

文件解析/编译失败时不替换项目自定义 Placeholder 资源。翻译执行期的匹配或保护冲突以
标准单元为最小失败范围；失败单元不发给 LLM，其他独立单元仍可规划。阶段如何记录部分
结果由[翻译规格](translation.md)规定，规则文件本身没有“忽略冲突”或优先级开关。

### 6.6 提供文件、略去参数与显式空数组的生命周期

- 提供非空 `--placeholders FILE`：完整替换项目自定义 Placeholder 资源；
- 省略 `--placeholders`：复用项目当前资源，不重新解析外部文件；
- 提供内容为 `rule = []` 的文件：清空自定义规则，但 Builtin 保护继续生效。

候选解析或编译失败时，项目中原有的自定义资源保持不变。

### 6.7 可复制正例、反例和物化结果

本节 6.1 的 TOML 是可复制正例；最小反例是 `pattern = ''`，它与 3.1 的空模式反例具有
相同编译失败语义。完整多 scope 文件见
[`examples/placeholders.toml`](examples/placeholders.toml)。第 6.9 节给出 wrapper、
NaturalText 与 Builtin 控制符共同物化的结果；测试时应同时覆盖命中 scope、不命中 scope、
opaque 壳和壳内正文。

### 6.8 MV/MZ Builtin 控制符矩阵

Builtin 匹配严格使用 ASCII 字母和 `[0-9]`；命令名接受 ASCII 大小写，不应用 Unicode
大小写折叠。反斜杠和括号都是游戏文本的实际字符。

| 控制符 | MV | MZ |
|---|:---:|:---:|
| `\V[n]`、`\N[n]`、`\P[n]`、`\C[n]`、`\I[n]` | 保护 | 保护 |
| `\PX[n]`、`\PY[n]`、`\FS[n]` | 不内建 | 保护 |
| `\G` | 保护 | 保护 |
| `\\`、`\{`、`\}`、`\$`、`\.`、`\|`、`\!`、`\>`、`\<`、`\^` | 保护 | 保护 |

`n` 必须由一个或多个 ASCII 数字组成。插件扩展，包括 MV 插件自行实现的 PX/PY/FS，
用自定义 Placeholder Rule 明确保护。

MV 行为依据 RPG Maker MV 的
[官方 `Window_Base` 核心脚本](https://raw.githubusercontent.com/rpgtkoolmv/corescript/master/js/rpg_windows/Window_Base.js)
固化；ATT 只内建该脚本中自己实际消费的上述控制符，不把插件新增控制符推断成 Builtin。

### 6.9 匹配、NaturalText 与重叠

完整匹配必须非零宽并对齐 UTF-8；`text` 若存在，必须参与、位于匹配内并对齐。自定义
规则零命中合法。ATT 先求出每条规则实际保护的跨度，再检查冲突：保护跨度相交才冲突。
wrapper 的 NaturalText 捕获中可以继续出现 Builtin 控制符，Builtin 会在 NaturalText
内部保护它，不会因为两个完整正则范围包含彼此就误判。

`Value` 是一个完整标量，规则可以按 PCRE2 设置跨其中的 LF 匹配。`Lines` 则保留元素槽
边界：无 `text` 捕获的完整 opaque 匹配不得跨元素；有 `text` 捕获时，完整 wrapper 匹配
可以跨元素，但 `<msg>`、`</msg>` 这类实际 opaque 前后壳必须各自位于单个元素，拼接 LF
只能留在 NaturalText 捕获中。

<!-- att-example: valid -->
```toml
[[rule]]
scopes = ["event_dialogue"]
pattern = '<msg>(?<text>.*?)</msg>'
```

对于 `<msg>勇者\C[2]</msg>`，`<msg>`/`</msg>` 是自定义 opaque wrapper，`勇者` 是
NaturalText，`\C[2]` 是 NaturalText 内的 Builtin 保护段。三者可以自然组合。

例如源 `Lines` 为 `["<msg>第一行", "第二行</msg>"]` 时，若模式启用 DOTALL，元素边界
位于 `text` 捕获中，因此合法；无 `text` 捕获并把两行整体保护的模式则使该单元规划失败。

Custom/Custom 或 Custom/Builtin 的实际保护跨度相交时，本单元规划失败；没有规则优先级、
最长匹配或静默覆盖。

### 6.10 token、FullyProtected 与 Current

原文中保留前缀 `⟦ATT_`，不能作为普通文本进入翻译，因为它属于 ATT token 命名空间。
token 只由实际选中的保护类别、segment 和源位置顺序生成；自定义规则编号不进入 token、
label 或 state。插入、删除、重排一条对该单元不命中的规则，不改变模型文本或 state。

若去掉全部 opaque 段后没有任何非空白 NaturalText，prepared 状态为
`fully_protected`，不请求 LLM；只剩空格、制表符或换行也属于这种情况。
模型响应必须精确保留 token 的数量和对齐位置。原文中两个字节完全相同的占位符仍是两个
独立槽；新响应若无法无歧义判断它们的对应关系，拒绝为
`placeholder_normalization_ambiguous`。

已存译文的 state 与当前事实精确匹配时直接判定 Current，不把恢复后的旧译文再次反向
正规化。因此重复相同占位符的已验收译文在第二次运行不会再次请求 LLM；严格歧义检查
只用于验收新的模型候选。

实际命中的 Placeholder 绑定（而非整份文件）进入 state。以下变化会使受影响单元失效：
保护跨度、类别、顺序或原始保护字节改变。未命中规则、规则诊断编号、并发和重试变化
不会使它失效。

## 7. Terminology 与协议壳的边界

术语只在 Placeholder 投影后的每段 NaturalText 内逐段匹配，不扫描 opaque 协议壳，也
不跨两个 `OpaqueBoundary` 拼接。Standard Prompt、Standard state 和 Lua
`prepared.terms` 复用同一次有序命中结果。

假设术语 trigger 是 `勇者`：

<!-- att-example: illustrative -->
```text
原文：<actor title="勇者">勇者</actor>
规则：<actor title="勇者">(?<text>.*?)</actor>

opaque 前壳中的“勇者”：不命中
NaturalText 捕获中的“勇者”：命中
opaque 后壳：不扫描
```

若 trigger 的前半在一个 NaturalText 段、后半在另一个 NaturalText 段，它也不命中。
术语文件完整契约见[术语现行规格与指南](terminology.md)。

## 8. 一次写对的验证清单

提交规则前，用当前游戏的正向和反向样本逐项确认：

1. 来源是实际启用的消费者读取的精确物理身份，文件大小写与目录项一致；
2. 正例能命中，形似的代码、资源名、禁用插件或不可达内容不命中；
3. 路径每一层的 JSON 类型和逐层解码都有真实样本；
4. 完整匹配、命名捕获、Literal 和最终写回边界符合协议；
5. group/unit 自然顺序与真实显示顺序一致；
6. Mutation Claim 没有与 Builtin、另一规则或 Lua 竞争；
7. 未翻译 round-trip 逐字保持，翻译 round-trip 只改变允许的槽；
8. 重复 Extract/Translate 收敛，未命中资源变化不制造无关重译。

完整阶段事务见[提取规格](extraction.md)，state 与验收见[翻译规格](translation.md)，
物化 recipe 的写回见[写回规格](write-back.md)。
