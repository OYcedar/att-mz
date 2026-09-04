# RPG Maker Extract 现行规格

Extract 从 Init 冻结的来源中提取文本，保存语义层次、原文和可逆写回关系。Builtin 处理本页
列出的标准字段，Rules 处理使用者明确选择的来源和路径；整个命令不发出模型请求。

```text
att mv extract --name NAME [--builtin] \
  [--dialogue-rules FILE] [--rules FILE]

att mz extract --name NAME [--builtin] [--rules FILE]
```

## 1. 运行方案

首次 Extract 必须显式选择 `--builtin`、`--rules FILE` 中至少一项。MV 的
`--dialogue-rules FILE` 只修饰 Builtin，不构成独立 owner。

项目保存最近一次成功采用的 Builtin/Rules 集合。之后省略全部提取选项时复用该集合；
显式提供任一 owner（资产所有者）时，本次集合精确替换旧方案，未列出的 owner 跳过执行，既有资产
原样保留。

- `rule = []` 的 Extract Rules 文件停用 Rules 并删除其资产；
- MV `rule = []` 的 dialogue rules 只清空姓名投影定义；
- 清理后没有可执行 owner 时，保存方案为空，下次无参数 Extract 会明确失败。

显式 `rule = []` 成功生效时，stdout 只保留 Extract 业务摘要；stderr 使用统一四字段警告
说明规则文件、Rules 已停用、资产已删除，以及运行方案是否仍有可执行 owner。同一事实写入
`diagnostic.extract`；不再另写一行 owner 停用或空方案提示。

各 owner 的提交彼此独立：Builtin 成功而后续 Rules 失败时，Builtin 的新结果落库，
旧 Rules 快照保持。

`run_plan.resolved` 保存本次 Builtin/Rules 选择；`run_plan.finalized` 保存运行方案是否成功写入
以及命令是否继续。某个 owner 失败时，可读诊断说明对象、来源、直接原因和修改方法；已经
提交的 owner 进度继续保留，不能被后续失败误报成整个 Extract 未发生。

## 2. 资产与身份

Extract 先按引擎结构建立明确的 `Semantic Scope → Group → Unit`。每个 Group 保存 kind、
来源语境、规范语义顺序和一个或多个 Unit；Translate 直接读取这个层次，不再从相邻路径
或 owner 类型推断 Scope。Unit 内容形状由 RPG Maker 字段决定：

- 单字符串；
- 固定逐行数组；
- 固定逐项数组；
- 可自由断行数组。

内部身份由来源位置和 Unit 角色确定，排序字段不参与身份。Builtin 与 Rules 都保存可从
冻结来源重新验证的写回 recipe。Rules Unit 还保存产生它的 TOML 自然规则序号，从 1 开始；
Builtin Unit 没有规则序号。Rules 命中标准事件参数时，Extract 还冻结实际命令编号，后续
按物理参数位置和命令编号继承内建控制规则。内部位置和顺序键不会进入 CLI、Manual、日志
或高级 Lua。

对人使用的 ID 由当前项目索引生成，例如：

```text
Skills.json:798:name
Map023.json:event17:page1:dialogue42
```

Rules 路径显示解码后的字段名和自然编号，JSON 解码步骤不单独显示；含空格或标点的字段名
使用 JSON 引号式方括号。程序按完整可读 ID 查当前索引，不解析字符串反推数据库位置。

Builtin 与 Rules 命中相同 kind 和 group location 时属于同一个 Group，读取时按完整顺序
合并，同时保留每个 Unit 的 owner。Group 顺序键不一致、kind 冲突、重复角色或不同对象
占用同一顺序键都会明确失败；owner 类型不作为排序补充。

## 3. Builtin 精确覆盖矩阵

Builtin 只读取下表中的标准字段。数据库数组里的 `null` 条目跳过；表中已列出的字段
必须存在且为字符串。空或纯空白字符串不建立 Unit，非空原文保持原样，不会先裁剪。

| 文件 | Builtin 字段 |
|---|---|
| `Actors.json[*]` | `name`、`nickname`、`profile` |
| `Classes.json[*]` | `name` |
| `Skills.json[*]` | `name`、`description`、`message1`、`message2` |
| `Items.json[*]` | `name`、`description` |
| `Weapons.json[*]` | `name`、`description` |
| `Armors.json[*]` | `name`、`description` |
| `Enemies.json[*]` | `name` |
| `States.json[*]` | `name`、`message1`、`message2`、`message3`、`message4` |

`System.json` 的覆盖范围为：

- 根字段 `gameTitle`、`currencyUnit`；
- `terms.basic[*]`、`terms.commands[*]`、`terms.params[*]`；
- `terms.messages` 对象中的每个成员，不限定键名；每个值都必须是字符串；
- 根数组 `elements[*]`、`skillTypes[*]`、`weaponTypes[*]`、`armorTypes[*]`、
  `equipTypes[*]`。

非空 `System.json.gameTitle` 只建立这一项 Builtin Unit。标准 NW.js `package.json` 的
`window.title` 与 `package.main` 活动 HTML 中实际且唯一的小写无属性 `<title>` 元素是该
Unit 的派生启动显示位置，不建立重复 Unit，也不进入 Rules 或 Generic ownership。

上述数组中的 `null` 槽跳过，其他槽必须是字符串。Map 与事件列表只读取：

| 来源 | Builtin 位置 |
|---|---|
| 规范命名的 `MapNNN.json` | 根 `displayName`；`events[*].pages[*].list` |
| `CommonEvents.json` | `[*].list` |
| `Troops.json` | `[*].pages[*].list` |

三个事件来源共用以下指令矩阵：

| code | Builtin 文本与结构 |
|---:|---|
| `101` + 连续 `401` | 对话块；每条 `401.parameters[0]` 是一行正文。MZ 把真值且非空白字符串的 `101.parameters[4]` 作为 Speaker；缺失或 JavaScript falsy 值表示没有 Speaker，真值非字符串无效。MV 不读取该参数，而是按可选的 dialogue rules 从第一条 `401` 投影 Speaker。孤立 `401` 是结构错误。 |
| `102`、同缩进 `402`、`404` | `102.parameters[0]` 是唯一的选项显示字符串数组。现有同缩进 `402` 只把整数 `parameters[0]` 与实际选择值比较；ATT 不要求它落在 102 数组范围内，不相等的分支由引擎自然跳过。它的其他参数是编辑器冗余数据，不要求与 102 文案相等，也不要求每个选项都存在 `402`。`404` 结束该块，`402` 不建立独立译文。全部选项为空白时整个块忽略。 |
| `105` + 连续 `405` | 滚动文本块；每条 `405.parameters[0]` 是一行正文。孤立 `405` 是结构错误。 |
| `320` | `parameters[1]`：角色新 `name`。 |
| `324` | `parameters[1]`：角色新 `nickname`。 |
| `325` | `parameters[1]`：角色新 `profile`。 |

MZ 的 `101.parameters[4]` 可以缺少；JSON 中可达的 JavaScript falsy 值为 `null`、`false`、数值零
和空字符串，它们都按没有 Speaker 处理。纯空白字符串同样不建立可翻译 Speaker。MV 从不
读取这个参数。`102` 的选项全部为空白时不建立 Unit；只要有一项非空，块内其他空白
选项仍作为位置槽保留。每条事件指令本身仍必须是对象，并具有整数 `code` 和数组
`parameters`；“不提取某个 code”不表示接受损坏的事件列表结构。

插件参数、插件命令 `356/357`、`note`/`meta`、`MapInfos.json`、自定义 `data/*.json`、事件
脚本 `355/655`、普通事件注释 `108/408`、JavaScript 以及其他未列出的字段和事件 code，均
不在 Builtin 覆盖范围内。这些来源先按 [Extract Rules](rules.md)核对；Rules 也无法形成
确定、完整、可逆的读写时，再转换为 JSONL 并建立 [Generic 项目](../generic/README.md)。
可见性、复杂度、数量和插件位置只是调查线索，来源选择仍取决于实际字段语义与读写关系。

Rules 的字段、来源、路径、捕获、顺序和错误范围由[规则规格](rules.md)定义。

MV/MZ `ownership export` 在一个只读快照中导出全部当前 Extract Unit 的 Builtin 或 Rules
所有权；Rules 行直接使用这里保存的自然规则序号，不根据可读 ID、路径前缀或相邻位置猜测。

## 4. 冲突、继承和提交

每项可写位置形成 Mutation Claim。同一物理值只容许一方声明互斥修改，祖先与后代路径
之间也不得形成覆盖歧义；任一冲突都会使当前 owner 候选失败。

owner 成功提交时：

- 身份和源语境仍相同的 Unit 继承自动译文与状态；
- 只有兄弟 Unit 使完整 Group 语境改变时，仍能唯一对应的 Unit 保留自动正文和原状态；该状态
  与新 Group 事实不匹配，因此不再 Current，也不参与 WriteBack 或后续模型语境；
- 自身原文、形状或写回关系改变的 Unit 不继承旧自动译文；
- 删除的 Unit 与自动状态一并删除；
- 新 Unit 为未翻译。

Extract 不删除 `rpg_maker_manual_translation`。位置不存在，或 Group kind、Unit 角色、写回
recipe、正文形状、原文或项目语言对改变时，人工记录成为过期，但旧原文和译文继续保留。
完整 Group 语境、相邻文本、术语、Placeholder 配置、Prompt、Profile 和 Client 不参与人工
适用性。绑定条件重新匹配、且通过当前强契约复验时，记录可以再次成为当前。

Extract 替换 owner 快照时，同一自然位置和角色上的 Rejected 候选及其状态随 Unit 原样保留。
只有与新事实精确匹配的当前适用性指纹才是 Current；绑定事实恢复后，原状态可以重新匹配。位置或角色已经
不存在时没有可挂接的当前 Unit，候选随旧 Unit 删除。

Extract 只负责提取，全程不发出模型请求。成功结果必须包含 owner、Group、Unit、冲突
摘要和来源指纹，供 Translate 与 WriteBack 重新检查。

Rules command 省略 path 后跳过非字符串参数时，可读警告说明规则文件、自然规则号、事件
命令、参数位置、实际类型、跳过数量和修改方法，不保存原始参数值或内部位置。警告不改变
成功提交和退出码。Extract 只有明确成功后才写 `phase.completed`；失败或取消写
`phase.stopped`，不得在失败路径出现完成事件。
