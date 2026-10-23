# RPG Maker 规则文件现行规格与编写指南

RPG Maker MV/MZ 的三类声明式规则由本规格定义：

- MV 对话姓名投影（`--dialogue-rules`）；
- Extract Rules（`extract --rules`）；
- Placeholder Rules 的 RPG Maker `scopes`、内建控制符与数组槽边界（`translate --placeholders`）。

Placeholder 的公共 TOML、捕获、token 与恢复契约见[公共 Placeholder 规格](../translation/placeholders.md)；提取与翻译阶段的状态交接分别见 [Extract](extraction.md) 与 [Translate](translation.md)；WriteBack 排版规则见 [WriteBack 排版规则规格](../translation/write-back-layout-rules.md)。

## 1. 三类文件解决不同问题

| 文件 | 命令位置 | 职责范围 | 不负责 |
|---|---|---|---|
| MV 姓名投影 | `mv extract --builtin --dialogue-rules FILE` | 标准 MV 对话第一条 `401` 中的 Speaker 投影 | 插件消息、任意文本提取、控制符保护 |
| Extract Rules | `mv|mz extract --rules FILE` | 已知来源、确定路径、最终字符串及可逆写回边界 | 动态键枚举、跨文档关系、一译多写 |
| Placeholder Rules | `mv|mz translate ... --placeholders FILE` | 已提取文本中不可让模型改写的协议跨度 | 新增提取位置、修改分组 |

需要动态键枚举、条件筛选、跨事件脚本拼接或一个译文写回多个目标时，转换为 [Generic JSONL](../generic/jsonl.md) 并使用独立 Generic 项目。

## 2. 共同根结构、严格解析与生命周期

三类文件均为严格 UTF-8 TOML，根必须且只能包含 `rule` 数组。非空定义使用一个或多个 `[[rule]]`；显式清空写作：

```toml
rule = []
```

零字节文件、仅含注释的文件、缺少 `rule`、未知字段、重复字段或错误类型均按无效输入拒绝。候选解析、编译或针对冻结来源执行失败时，旧定义与旧快照保持不变。

文件参数的生命周期：

| 输入方式 | MV 姓名投影 | Extract Rules | Placeholder Rules |
|---|---|---|---|
| 提供非空文件 | 完整替换项目姓名定义，并与本次 MV `--builtin` 一起执行 | 完整执行并原子替换 Rules owner | 完整替换自定义占位符资源 |
| 提供 `rule = []` | 清空姓名定义 | 停用并删除 Rules owner 快照（输出四字段成功警告，退出码 `0`） | 清空自定义规则；Builtin 控制符保护继续生效 |
| 省略参数 | 复用项目当前定义，不重新读取外部文件 | 全部 owner 参数均省略时复用上次成功方案；显式选择其他 owner 时本次不执行且既有资产不变 | 复用项目当前资源 |

每条非空 Extract Rule 在 TOML 中的数组位置即其从 1 开始的自然规则序号，物化出的每个 Unit 保存该序号并由 `ownership export` 原样导出。

### 2.1 PCRE2 与三层转义

三类规则的正则均使用 PCRE2（开启 UTF 与 UCP）：模式内联标志（如 `(?i)`、`(?s)`），不使用 `/.../flags` 字面量。`.` 默认不匹配 LF。推荐使用 TOML 单引号字面字符串书写正则与路径，使反斜杠原样传给 PCRE2 或路径解析器：

```toml
[[rule]]
order = 'preserve'
pattern = '\\SE\[[^]]+\]'
```

Extract 路径中的 quoted key 叠加 TOML 字符串与 JSON string 两层语法；例如对象键为 `a"b` 时，用 TOML 单引号包裹并按 JSON string 转义双引号：

```toml
path = '["a\"b"]'
```

## 3. MV 对话姓名投影

`--dialogue-rules` 仅与 `mv extract --builtin` 同时使用，扫描标准 MV 事件消息块 `101 + 连续 401*` 的第一条 `401.parameters[0]`（MZ 使用 `101.parameters[4]` 原生 Speaker，不读取此文件）。完整多规则示例见 [`examples/mv-dialogue.toml`](examples/mv-dialogue.toml)。

### 3.1 字段与编译约束

每条规则只允许唯一必填字段 `pattern`（非空 PCRE2 字符串，必须恰好包含一个名为 `speaker` 的命名捕获，不允许其他命名捕获）：

```toml
[[rule]]
pattern = '\A\\N<(?<speaker>[^>]*)>\z'
```

### 3.2 匹配与投影规则

- 每个完整匹配必须非零宽、位于 UTF-8 字符边界；`speaker` 必须参与匹配且完全位于该匹配内。
- `speaker.trim().is_empty()` 为真时不建立 Speaker（仅冻结已匹配外壳）；非空白时建立 Speaker，捕获的原始字节（含首尾空白）原样保存与写回。
- 同一行内同一规则可产生多个不重叠匹配，但实际建立的 Speaker 原始字节必须完全相同；两条不同规则命中同一第一条 `401` 即冲突。
- 每条非空规则在当前冻结来源中必须至少建立一个非空白 Speaker，否则整个 Builtin 候选失败。
- ATT 将从行首到最后一个 marker 结束处投影为 Literal/SpeakerSlot；最后 marker 之后若含非空白字符则整体作为该行 Body，若为空或纯空白则冻结为 Literal：

```text
源第一行：\N< 勇者 >   你好
pattern ：\A\\N<(?<speaker>[^>]*)>
Speaker ：" 勇者 "        # 原始空格保留
Body    ："   你好"        # 后缀含非空白，整体进入 Body

源第一行：\N<>   
Speaker ：不存在           # 捕获 trim 后为空
Literal ："\N<>   "       # 纯空白后缀一并冻结
Body    ：不由这一行建立
```

## 4. Extract Rules

一份 Extract Rules 文件整体解析、编译并针对冻结来源执行，作为一个 Rules owner 候选原子提交；任一规则失败或发生 Mutation Claim 冲突时，整个候选不提交。可复制的三来源示例见 [`examples/extract-rules.toml`](examples/extract-rules.toml)。

### 4.1 字段与来源起点

```toml
[[rule]]
file = 'QuestEntries.json'
path = '[].title'
decode_json = false
pattern = '\A\[title\](?<text>.+)\z'
```

| 字段 | 类型 | 必填/默认 | 约束与 `path` 起点 |
|---|---|---|---|
| `file` | string | 三类来源互斥选一 | 安全精确 `.json` 基名或唯一通配 `Map*.json`；`path` 从文件 JSON 根值开始 |
| `plugin` | string | 三类来源互斥选一 | `js/plugins.js` 中精确名称且 `status = true` 的插件；`path` 从该插件的 `parameters` 对象开始 |
| `code` + `parameter` | 非负整数对 | 三类来源互斥选一 | 扫描规范 Map、CommonEvents、Troops 的事件命令；`path` 从 `parameters[parameter]` 开始 |
| `path` | string | `file`/`plugin` 必填；`code` 可省略 | 非空确定路径；`code` 省略 `path` 时直接取 `parameters[parameter]` 作为终点 |
| `decode_json` | boolean | 可选，默认 `false` | 所有路径步骤完成后对终点 string 额外解码一次 JSON，结果仍须为 string |
| `pattern` | string | 可选 | 非空 PCRE2，必须恰好包含一个 `text` 命名捕获；省略时使用整个最终 string |

### 4.8 文件身份、Map 和实际大小写

精确 `file` 必须是单个 UTF-8 基名（不含路径分隔符、控制字符、`<>:"/\|?*` 或 Windows 保留设备名 `CON`/`PRN`/`AUX`/`NUL`/`CONIN$`/`CONOUT$`/`COM1`..`COM9`/`LPT1`..`LPT9`/`COM¹`..`COM³`/`LPT¹`..`LPT³`），以小写 `.json` 结尾，且与 `data/` 实际目录项逐字同大小写。

规范 Map 文件名为 `Map` + 1～4,294,967,295 (`u32::MAX`) 的十进制 ID（至少三位，无多余前导零）+ `.json`。`Map*.json` 只遍历规范 Map；`Map000.json`、`Map01.json`、`Map0001.json` 等若作为精确 `file` 提供，按普通自定义 DataFile 处理。

### 4.9 路径 EBNF 与 quoted key

```ebnf
path          = segment, { (".", bare-key) | bracket-step } ;
segment       = bare-key | bracket-step ;
bracket-step  = "[]" | "[", index, "]" | "[", json-string, "]" ;
bare-key      = (ASCII-letter | "_"), { ASCII-letter | ASCII-digit | "_" } ;
index         = ASCII-digit, { ASCII-digit } ;
json-string   = JSON string token, including its double quotes ;
```

- `[3]` 为固定零基数组下标，`[]` 按数值顺序展开全部非 `null` 元素，`["..."]` 为精确对象键（采用 JSON string 转义，`path = '[""]'` 合法并选择空键；不支持 `$`、递归下降、通配或负数下标）。
- 路径尚未结束而当前值为 JSON string 时，ATT 自动将该 string 解码为 JSON 后继续深入（每层解码均记入写回 recipe）；`decode_json = true` 仅在路径到达终点后额外解码一次 string。

### 4.10 整串、局部捕获、顺序与分组

省略 `pattern` 时，最终 string 整体作为一个 Scalar Unit。提供 `pattern` 时：

- 完整匹配与 `text` 捕获均须参与、非零宽并落在 UTF-8 字符边界；
- 同一最终 string 内的多次匹配按 `text` 起始字节升序排列且不得重叠，非空白 `text` 共同形成一个 Group，匹配外与捕获外的字节冻结为 recipe Literal：

```text
源值  ：A<t>第一段</t>B<t>第二段</t>C
pattern：<t>(?<text>.*?)</t>
物化组：Unit 0 = "第一段", Unit 1 = "第二段"
recipe：Literal("A<t>") + Slot(0) + Literal("</t>B<t>") + Slot(1) + Literal("</t>C")
```

### 4.11 来源执行与原子失败范围

- **路径分支与类型检查**：对象缺 key 或固定数组下标越界只使当前分支无产出；但要求 object/array 却遇到其他类型、JSON 解码失败或终点不是 string 会使整个 Rules 候选失败。
- **事件命令扫描**：非 object 命令项或非整数 `code` 跳过；整数 `code` 命中后，`parameters` 非数组或缺少指定 `parameter` 下标会使整个候选失败。`code` 规则省略 `path` 且直接参数为非字符串时，按规则号、来源、命令、参数下标和实际类型聚合为 stderr 警告并跳过（不改变退出码 `0`）。每条 `code` 规则只处理单条命令的单个参数，不跨多条 `355/655` 拼接脚本。
- **最少产出要求**：每条非空规则在整个冻结来源中必须至少产出一个非空白 string 单元，否则以 `rules_no_non_blank_match` 失败。

## 5. 语义范围、自然顺序与 Mutation Claim

普通数据库文件、`System.json`、每张 Map、每个 CommonEvent、每个 Troop 和每个启用插件各自形成一个 Semantic Scope（TaskBlock 不跨 Scope）。Builtin 与 Rules 命中相同逻辑位置且 kind 相同时合并为同一 Group。

Rules 跨来源顺序固定为：

1. **标准 DataFile**：`Actors.json → Animations.json → Armors.json → Classes.json → CommonEvents.json → Enemies.json → Items.json → MapInfos.json → Skills.json → States.json → System.json → Tilesets.json → Troops.json → Weapons.json`；
2. **自定义 DataFile**（含显式指定的非规范 Map 名）：UTF-8 基名字典序；
3. **规范 Map**：MapId 数值升序；
4. **`plugins.js`**：插件数组下标升序，插件内保持 `parameters` 成员顺序。

JSON 内部保持对象成员顺序、数组数值下标与捕获起始字节顺序。

ATT 从每个可写位置及事件块 recipe 派生 `MutationClaim`（穿过路径为 `Intent`，拥有可变终点为 `Exclusive`）：

| 两个声明的关系 | 结果 |
|---|---|
| 同一 raw JSON Value，或 raw JSON string 与其解码后的任意后代 | 冲突 |
| Dialogue/Choices/ScrollingText 事件块与其覆盖字段或解码后代 | 冲突 |
| 同一已解码对象中的不同 sibling，或互不覆盖的事件块/普通值 | 允许 |

## 6. RPG Maker Placeholder Rules

本节补充 [公共 Placeholder 规格](../translation/placeholders.md) 在 MV/MZ 下的作用域、内建控制符与数组槽约束。完整示例见 [`examples/placeholders.toml`](examples/placeholders.toml)。

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

RPG Maker 的合法 `scopes` 固定为八个：`database_entry`、`system`、`map`、`event_dialogue`、`event_choices`、`event_scrolling_text`、`event_command`、`plugin_parameter`（省略 `scopes` 时适用于全部八个 kind；与可选 `ids` 取交集）。

### 6.8 内建控制符

ATT 根据项目引擎（MV/MZ）与 Unit 的标准物理字段自动保护以下内建控制符（Rules 命中标准事件参数时按物理参数位置和命令编号继承相同规则；插件参数或自定义文件不自动套用）：

| 语法族 | MV | MZ | 标准字段消费者条件 |
|---|:---:|:---:|---|
| `%[0-9]+` | 有 | 有 | `String.prototype.format` 格式字符串（允许同一槽内重排） |
| `\\`、`\V[n]`、`\N[n]`、`\P[n]`、`\G`、`\C[n]`、`\I[n]`、`\{`、`\}` | 有 | 有 | 扩展文字消费者（保持相对顺序） |
| `\PX[n]`、`\PY[n]`、`\FS[n]` | 无 | 有 | MZ 扩展文字消费者（保持相对顺序） |
| `\$`、`\.`、`\|`、`\!`、`\> `、`\<`、`\^`、U+000C | 有 | 有 | Message 消费者（保持相对顺序） |

- 反斜杠与 U+001B 按引擎顺序成对转义（每对保护为一个字面反斜杠，剩余单个反斜杠引出后续控制命令）。
- `\N<...>`、`\N1<...>` 等 inline 姓名框不属于 MV/MZ 通用内建语法；若游戏插件使用此类语法，通过带 `text` 捕获的 Custom Placeholder 保护前后壳，或通过 MV `--dialogue-rules` 投影。

### 6.9 匹配、NaturalText 与重叠

- 实际保护跨度相交时（Custom/Custom 或 Custom/Builtin）规划失败；但带 `text` 捕获的 wrapper 仅保护前后壳，其 `text` 内部可自然包含 Builtin 控制符（如 `<msg>勇者\C[2]</msg>`）。
- 标量 `Value` 允许正则跨内部 LF 匹配；多行数组 `Lines` 保留元素槽边界：无 `text` 捕获的完整 opaque 匹配不得跨元素，带 `text` 捕获的 wrapper 前后壳必须各自位于单个元素内。
- 原文不得包含保留前缀 `⟦ATT_`。去掉全部 opaque 段后无非空白 NaturalText 的 Unit 记为 `fully_protected`，不请求模型。

## 7. Terminology 与协议壳的边界

术语匹配只在 Placeholder 投影后的每段 NaturalText 内独立执行，不扫描 opaque 协议壳，也不跨两个 `OpaqueBoundary` 拼接匹配（详见[公共术语规格](../translation/terminology.md)）。
