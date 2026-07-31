# RPG Maker Extract 现行规格

```text
att mv extract --name NAME [--builtin] \
  [--dialogue-rules FILE] [--rules FILE]

att mz extract --name NAME [--builtin] [--rules FILE]
```

Extract 的执行者是 Builtin 与 Rules 两类能力，Lua 不在其中。

## 1. 运行方案

首次 Extract 必须显式选择 `--builtin`、`--rules FILE` 中至少一项。MV 的
`--dialogue-rules FILE` 只修饰 Builtin，不构成独立 owner。

项目保存最近一次成功采用的 Builtin/Rules 集合。之后省略全部提取选项时复用该集合；
显式提供任一 owner 时，本次集合精确替换旧方案，未列出的 owner 跳过执行，既有资产
原样保留。

- `rule = []` 的 Extract Rules 文件停用 Rules 并删除其资产；
- MV `rule = []` 的 dialogue rules 只清空姓名投影定义；
- 清理后没有可执行 owner 时，保存方案为空，下次无参数 Extract 会明确失败。

各 owner 的提交彼此独立：Builtin 成功而后续 Rules 失败时，Builtin 的新结果落库，
旧 Rules 快照保持。

## 2. 资产与身份

每个 Group 保存 kind、来源语境、自然顺序和一个或多个 Unit。Unit 内容形状由 RPG Maker
字段决定：

- 单字符串；
- 固定逐行数组；
- 固定逐项数组；
- 可自由断行数组。

Unit 身份是 `owner + group_location + unit_role`，排序字段不参与身份。Builtin 与
Rules 都保存可从冻结来源重新验证的写回 recipe。

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

上述数组中的 `null` 槽跳过，其他槽必须是字符串。Map 与事件列表只读取：

| 来源 | Builtin 位置 |
|---|---|
| 规范命名的 `MapNNN.json` | 根 `displayName`；`events[*].pages[*].list` |
| `CommonEvents.json` | `[*].list` |
| `Troops.json` | `[*].pages[*].list` |

三个事件来源共用以下指令矩阵：

| code | Builtin 文本与结构 |
|---:|---|
| `101` + 连续 `401` | 对话块；每条 `401.parameters[0]` 是一行正文。MZ 另把存在且非空白的 `101.parameters[4]` 作为 Speaker；MV 不读取该参数，而是按可选的 dialogue rules 从第一条 `401` 投影 Speaker。孤立 `401` 是结构错误。 |
| `102`、同缩进 `402`、`404` | `102.parameters[0]` 是选项字符串数组；只要存在非空白选项，每个 `402.parameters[0]` 就必须是选项下标，`parameters[1]` 必须与对应选项完全一致，`404` 结束该组且所有分支必须完整出现。`402` 不建立独立译文。全部选项为空白时整个块忽略。 |
| `105` + 连续 `405` | 滚动文本块；每条 `405.parameters[0]` 是一行正文。孤立 `405` 是结构错误。 |
| `320` | `parameters[1]`：角色新 `name`。 |
| `324` | `parameters[1]`：角色新 `nickname`。 |
| `325` | `parameters[1]`：角色新 `profile`。 |

MZ 的 `101.parameters[4]` 可以缺少；存在时必须是字符串，纯空白表示没有 Speaker。MV
从不读取这个参数。`102` 的选项全部为空白时不建立 Unit；只要有一项非空，块内其他空白
选项仍作为位置槽保留。每条事件指令本身仍必须是对象，并具有整数 `code` 和数组
`parameters`；“不提取某个 code”不表示接受损坏的事件列表结构。

Builtin 明确不读取插件参数、插件命令 `356/357`、`note`/`meta`、`MapInfos.json`、任意
自定义 `data/*.json`、事件脚本 `355/655`、普通事件注释 `108/408`、普通 JavaScript 或其他
未列出的字段与事件 code。已知结构中的这些内容应使用 [Extract Rules](rules.md)；需要把
独立材料整理为 JSONL 时，建立独立的 [Generic 项目](../generic/README.md)。Builtin 没有
插件文本分支，也不会因为内容看起来像玩家可见文本而推断字段语义。

Rules 的字段、来源、路径、捕获、顺序和错误范围由[规则规格](rules.md)定义。

## 4. 冲突、继承和提交

每项可写位置形成 Mutation Claim。同一物理值只容许一方声明互斥修改，祖先与后代路径
之间也不得形成覆盖歧义；任一冲突都会使当前 owner 候选失败。

owner 成功提交时：

- 身份和源语境仍相同的 Unit 继承译文与状态；
- 原文、形状、Group 语境或写回关系改变的 Unit 清除旧译文；
- 删除的 Unit 与状态一并删除；
- 新 Unit 为未翻译。

Extract 只负责提取，全程不发出模型请求。成功结果必须包含 owner、Group、Unit、冲突
摘要和来源指纹，供 Translate 与 WriteBack 重新检查。
