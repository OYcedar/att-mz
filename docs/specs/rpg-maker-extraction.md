# RPG Maker 文本提取现行规格

本文定义 MZ 与 MV 共用的提取能力。一次命令可以组合 Builtin、Rules 和 Lua，执行顺序
固定为 `Builtin → Rules → Lua`；至少选择一项。三个 owner 分别原子替换自己的快照，
不会互相清理资产。

## 1. 命令与项目状态

```text
att --config FILE mz extract --name NAME \
  (--builtin | --rules RULES_TOML | --lua SCRIPT_LUA)+

att --config FILE mv extract --name NAME \
  (--builtin | --rules RULES_TOML | --lua SCRIPT_LUA)+ \
  [--dialogue-rules DIALOGUE_TOML]
```

`--dialogue-rules` 只属于 MV，并且必须与 `--builtin` 同时使用。提供文件时，它完整替换
项目当前 MV 对话定义；省略时 Builtin 复用项目状态；`rule = []` 明确清空定义。定义与
Builtin 资产快照在同一个数据库事务中提交。WriteBack 只消费物化后的 recipe 和项目
定义，不重读 TOML。

命令按 `<projects.root>/<engine>/<name>` 开启项目，取得项目租约并校验冻结来源指纹。
任一 owner 失败时，该 owner 的旧状态保持不变；先前已成功提交的其他 owner 不回滚。

## 2. 共享标准资产模型

每个提取结果由三层事实组成：

```text
Group
├─ group_location + group_kind
├─ TextProjectionRecipe
├─ Leaf(field_role, original_text, translation_context)
└─ MutationTarget（一个或多个物理修改目标）
```

逻辑叶身份固定为 `owner + group_location + field_role`。`TextFieldRole` 包含：

- `Scalar`；
- `DialogueSpeaker`；
- `DialogueBody(index)`；
- `ScrollingTextBody(index)`。

物理 JSON 地址只用于冻结来源复核和写回，不再充当译文身份。Group 保存完整 recipe，
包括没有形成翻译叶但写回时必须保留的空白行、Literal、SpeakerSlot 和原始命令边界。
每次 owner 替换都计算完整资产快照指纹，并原子写入：

- `standard_text_group`；
- `standard_text_leaf`；
- `standard_text_target`；
- `standard_asset_owner_state` 中的来源与资产快照指纹。

同一物理目标只能归属一个 owner/group；跨 owner 或跨规则冲突在写入前失败。

## 3. Builtin 与对话差异

Builtin 共用数据库条目、System、Map、CommonEvents、Troops、事件列表、选择项、滚动
文本及插件目录遍历。固定字段包括 RPG Maker 标准名称、描述、消息、选择项、滚动文本
和已确认的事件文本字段；额外引擎/插件数据通过 Rules 或 Lua 提取。

标准消息块是 `101 + 连续 401*`。每个块建立一个 Dialogue Group，并记录所有连续
`401`，包括空白行或没有翻译叶的行。差异仅在 Speaker 投影：

- MZ 从 `101.parameters[4]` 读取可选原生 Speaker；参数缺失、空字符串或全空白均表示
  没有 Speaker。非空 Speaker 使用 direct target；
- MV 标准 `101` 没有原生 Speaker，只在第一条 `401.parameters[0]` 上应用项目当前
  姓名投影定义。其余对话结构与 MZ 使用同一 recipe、翻译与写回能力。

正文叶保持为 `DialogueBody(0..n)`，不能用一个“整组正文”叶替代逐叶身份。
滚动文本按 `105 + 连续 405*` 建组；所有 `405` 都进入物理 recipe，空白行不建立逻辑叶，
后续 `ScrollingTextBody(index)` 仍使用原物理行索引而不压缩。

## 4. MV 姓名投影 TOML

姓名文件只解释标准对话块第一条 `401.parameters[0]`。非空定义只需编写 PCRE2：

```toml
[[rule]]
pattern = '(?i)\\n<(?<speaker>[^>]*?)(?::)?>'

[[rule]]
pattern = '\A(?<speaker>バニー淫魔)\z'
```

显式清空使用另一份完整文件：

```toml
rule = []
```

根必须显式声明 `rule`。零字节、仅注释、未知或重复字段均无效。每条非空规则必须：

- 只有一个名为 `speaker` 的命名捕获，不接受 `text` 或其他命名捕获；
- 在当前冻结来源中至少捕获一个非空 Speaker，否则整个 Builtin 替换失败；
- 不产生零宽匹配或零宽 Speaker。

同一第一行可以有多个不重叠匹配，但所有非空 Speaker 必须逐字相同；同一物理字段只能
由一条规则拥有，跨规则完整匹配重叠失败。空 Speaker 的外壳原样冻结，不建立 Speaker
叶。

第一行从开头到最后一个完整匹配结束处被物化为 `Literal/SpeakerSlot`；最后一个匹配
之后的后缀才是该行 Body。marker 前的控制符、空白及重复 marker 之间的外壳逐字冻结。
若整条第一行被姓名匹配，该物理行只有 Speaker，后续 `401` 仍属于同一组正文。畸形
近似值保持普通 Body，不猜测修复。完整匹配与 `speaker` 捕获必须有序、位于原字符串
内并对齐 UTF-8 边界，捕获还必须完整位于对应匹配内；否则对话定义候选失败。

## 5. Extract Rules TOML

Rules 只从明确来源选择最终字符串或其中的 `text` 跨度，并立即物化可逆 recipe。非空
定义使用一个数组，通过互斥字段选择来源：

```toml
[[rule]]
file = "Disciplines.json"
path = '[].Name'

[[rule]]
plugin = "YEP_QuestJournal"
path = '["Quest 1"].Title'

[[rule]]
code = 356
parameter = 0
pattern = '(?i)\AGabText\s+(?<text>.+)\z'

[[rule]]
code = 357
parameter = 3
path = 'dText'

[[rule]]
file = "Classes.json"
path = '[].note'
pattern = '(?ms)<DESC:(?<text>.*?)>'

[[rule]]
plugin = "Mano_InputConfig"
path = 'GamepadIsNotConnected'
decode_json = true
```

显式停用 Rules owner 使用另一份完整文件：

```toml
rule = []
```

`rule = []` 停用 Rules owner 并清理其资产。每条非空规则必须且只能选择以下一种来源：

1. `file`：安全的精确 `.json` 基名、精确 `MapNNN.json` 或 `Map*.json`；
2. `plugin`：启用插件的参数对象；
3. `code + parameter`：Map、CommonEvents 与 Troops 中任意非负事件 code 的指定参数。

`file` 和 `plugin` 必须给出 `path`。命令参数可以直接成为终点，也可以继续给出 `path`。
不对 code 356、357 或其他数值附加硬编码语义。

路径只支持对象 key、固定数组 index、`[]` 展开和精确带点 key，例如：

```text
A.B
A[3].B
[].field
["exact.key"].value
```

路径不支持 JSONPath、递归、过滤器、对象 key 正则或其他通配符。路径需要继续深入而
当前值是字符串时，ATT 将其作为 JSON 自动逐层解码；每个边界进入物理 recipe，并在
写回时按相反顺序编码。`decode_json = true` 只控制最终字符串再解码一次。

`pattern` 缺席时翻译整个最终字符串；存在时必须有且只有一个 `text` 命名捕获，其他
命名捕获无效。一个字符串可以产生多个按来源顺序排列且不重叠的匹配；非空捕获形成
逻辑叶，其余内容冻结为 Literal。零宽、不参与、重叠捕获、跨规则重复物理目标都使
整个 Rules 候选失败。完整匹配与捕获必须有序、位于原字符串内并对齐 UTF-8 边界，
`text` 还必须完整位于对应匹配内。提交前必须使用原始叶逐字重建最终字符串。

每条非空规则在当前来源中必须产生至少一个非空翻译叶，否则整个 Rules 替换失败并
保留旧快照。规则顺序只用于诊断编号，不构成优先级。外部契约不包含 `label`、
`field_name`、`required`、`priority`、版本或跨命令状态字段。

## 6. Lua 与并发

Extract Lua 获得公共 `ctx.project/json/source/rpg_maker/db` 和阶段专属 `ctx.extract`；
`translation`、`llm`、`output` 与 `write_back` 为 nil。`ctx.extract.replace_standard`
原子替换 Lua owner，`clear_standard` 停用该 owner。脚本可以使用 `ctx.rpg_maker` 的
data、map、plugin、document 与位置能力，不需要复制 JSON/路径 codec。

文档磁盘读取使用 `[rpg_maker.document].read_concurrency`；JSON/PCRE2 处理与资产编码
全部进入命令私有 CPU 根，Store 配置只决定每个编码作业的批量粒度。读取完成的文档可
立即交给 CPU，不等待较早文件；Builtin 和 Rules 的独立局部工作使用有序批量计算，归并
始终按 RPG Maker 权威身份和自然顺序确定。完成顺序不改变逻辑位置、冲突结果或提交
内容。

## 7. 完成语义

同一 owner 的来源指纹、项目定义、组、叶、目标和资产快照指纹在一个事务中校验并
替换；失败不暴露半快照。相同快照返回 `Unchanged`，真实替换返回更新摘要。

成功摘要统计逻辑组和逻辑叶，不把物理位置数混入叶计数。审计位置使用
`LogicalTextLocation`；物理 Mutation Target 仅在内部错误定位和 WriteBack 配方中存在。
