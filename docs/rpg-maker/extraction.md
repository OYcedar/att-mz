# RPG Maker 文本提取现行规格

本文定义 MV/MZ Extract 的生命周期、标准资产、Builtin 覆盖、顺序与事务。三类声明式
文件的字段和错误由[规则文件现行规格](rules.md)唯一规定；Lua 形状由
[Lua 技术参考](lua.md)规定。

## 1. 命令与 owner 生命周期

<!-- att-example: illustrative -->
```text
att --config FILE mz extract --name NAME \
  (--builtin | --rules RULES_TOML | --lua SCRIPT_LUA)+

att --config FILE mv extract --name NAME \
  (--builtin | --rules RULES_TOML | --lua SCRIPT_LUA)+ \
  [--dialogue-rules DIALOGUE_TOML]
```

一次命令至少选择 Builtin、Rules、Lua 之一，执行和 owner 总顺序固定为
`Builtin → Rules → Lua`。三个 owner 分别原子替换自己的快照，不清理未选择 owner。
首个技术失败阻止后续 owner；此前已成功提交的 owner 不做组合回滚。

`--dialogue-rules` 只属于 MV 且必须同时选择 `--builtin`。提供文件完整替换姓名定义，
省略时复用项目定义，`rule = []` 明确清空。定义与 Builtin 快照同事务提交。Rules 参数
省略表示本次不执行 Rules；只有提供 `rule = []` 才停用 Rules owner。WriteBack 只消费
已物化 recipe，不重读 TOML 或正则。

命令按 `<projects.root>/<engine>/<name>` 取得项目租约，验证当前 schema 和冻结来源指纹。

## 2. 共享标准资产与当前 schema

<!-- att-example: illustrative -->
```text
Owner
└─ Group(group_order, group_location, group_kind, projection_recipe)
   ├─ Unit(unit_order, unit_role, source_content, source_context,
   │      translation, translation_state)
   └─ MutationClaim(resource_key, access = intent | exclusive)
```

语义身份是 `owner + group_location + unit_role`；`group_order` 和 `unit_order` 是非身份字段。
`group_order` 在 owner 内从 0 连续，`unit_order` 在组内从 0 连续。内容形状只有：

- `Value(String)`：Scalar 和 DialogueSpeaker；
- `Lines(Vec<String>)`：DialogueBody、Choices、ScrollingText。

Lines 的元素边界属于翻译事实；元素不得含 CR、LF 或 NUL。`source_context` 当前由
DialogueBody 保存源 Speaker，其余为空对象。译文内容必须与原文形状相同，译文与
translation state 必须成对存在或成对为空。Lua 私有表使用的 64 字符十六进制表示见
[Lua Translate](lua.md#7-translatepreparecurrent-与-accept)。

当前数据库相关表为：

- `standard_asset_owner_state`：owner 的来源与资产快照指纹；
- `standard_text_group`：含 `group_order` 的组与完整 recipe；
- `standard_text_unit`：含 `unit_order` 的语义单元、译文和 state；
- `standard_mutation_claim`：`owner + group_location + resource_key + access`；
- `standard_translation_resource`：术语与自定义占位符 canonical 资源；
- `standard_project_definition`：MV 姓名投影定义。

当前 schema 一次性替换旧 schema，不提供迁移、识别或兼容读取。旧项目应在项目根外备份
后重新 Init/Extract/Translate；不符合当前 schema 的数据库只作为普通无效项目数据库。

## 3. Builtin 覆盖矩阵

Builtin 只覆盖下表明确列出的玩家文本。字段按表中顺序建立 unit；数组按数值下标顺序，
对象按来源结构顺序。空白字段是否产出遵循对应标准资产语义，不扩展为“遍历所有 string”。

| 来源 | `group_kind` / Placeholder scope | 字段或结构（声明顺序） |
|---|---|---|
| `Actors.json` | `database_entry` | `name`、`nickname`、`profile` |
| `Classes.json` | `database_entry` | `name` |
| `Skills.json` | `database_entry` | `name`、`description`、`message1`、`message2` |
| `Items.json` | `database_entry` | `name`、`description` |
| `Weapons.json` | `database_entry` | `name`、`description` |
| `Armors.json` | `database_entry` | `name`、`description` |
| `Enemies.json` | `database_entry` | `name` |
| `States.json` | `database_entry` | `name`、`message1`、`message2`、`message3`、`message4` |
| `System.json` 根 | `system` | `gameTitle`、`currencyUnit` |
| `System.json.terms` | `system` | `basic[]`、`commands[]`、`params[]`、`messages` 中所有 string |
| `System.json` 类型数组 | `system` | `elements[]`、`skillTypes[]`、`weaponTypes[]`、`armorTypes[]`、`equipTypes[]` |
| 规范 `MapNNN.json` 根 | `map` | `displayName` |
| Map/CommonEvents/Troops `101 + 401*` | `event_dialogue` | 可选 Speaker、完整有序 Body |
| Map/CommonEvents/Troops `102`，对应同层 `402/404` | `event_choices` | 完整有序选择数组；写回同时维护分支标签 |
| Map/CommonEvents/Troops `105 + 405*` | `event_scrolling_text` | 完整有序滚动文本 |
| Map/CommonEvents/Troops `320` | `event_command` | `parameters[1]`（角色名） |
| Map/CommonEvents/Troops `324` | `event_command` | `parameters[1]`（昵称） |
| Map/CommonEvents/Troops `325` | `event_command` | `parameters[1]`（简介） |

Builtin **不**翻译 `Animations.json`、`MapInfos.json`、`Tilesets.json`、`js/plugins.js`、
插件自定义文件、任意 note/meta 或未列出的标准字段。文件可被项目冻结、Lua 打开或 Rules
精确选择，不代表 Builtin 会翻译它。

### 3.1 对话、选项和滚动文本

标准消息块是 `101 + 连续 401*`。MZ 从可选 `101.parameters[4]` 建立原生 Speaker；缺失、
空或全空白表示没有 Speaker。MV 按项目当前姓名投影处理第一条 `401`，精确语义见
[规则文件](rules.md#3-mv-对话姓名投影)。全部正文形成一个 DialogueBody，正文中的空白
`401` 作为 Lines 空元素保留；全空正文不建立 Body。

一个 `102` 的完整选项数组形成一个 Choices，包括空槽；recipe 同时声明对应同 indent
的 `402.parameters[1]` 和终止 `404`。滚动文本按 `105 + 连续 405*` 建组，全部 `405`
形成一个 ScrollingText，包括空行。`320/324/325` 只取参数 1。

## 4. Rules 与 Lua 的 group kind 映射

Rules 的完整字段契约、路径、逐层解码和失败范围见[规则文件第 4 节](rules.md#4-extract-rules)。
其来源自动决定 `group_kind` 与 Placeholder scope：

| Rules 来源 | `group_kind` / scope |
|---|---|
| 精确 `System.json` | `system` |
| 规范 Map 或 `Map*.json` 中的 Map | `map` |
| 其他精确标准/自定义 DataFile（包括近似 Map 名） | `database_entry` |
| 启用插件参数 | `plugin_parameter` |
| `code + parameter` 事件来源 | `event_command` |

Lua `replace_standard` 显式声明 kind，但 Host 会按同一来源矩阵校验；详见
[Lua Extract 矩阵](lua.md#6-extractreplace_standard)。

## 5. 自动分组和自然顺序

Builtin 数据库对象以对象条目为组，System 各逻辑数组/对象、每张 Map、每个事件块分别
按语义分组。Rules 沿路径展开后，以最终 string 的稳定父容器建立组；同一 string 中
同一规则的多次 `text` 捕获合并为一个组，并按捕获起始字节形成 sibling fields。Lua
严格使用 `groups`/`fields` 无洞数组声明顺序；同一次 `replace_standard` 中的
`(group.location, group.kind)` 必须唯一，重复项直接失败，不会交给共享归一化自动合并。

自然顺序为：

1. owner：Builtin、Rules、Lua；
2. Builtin：来源结构、字段规格声明、数组数值下标；
3. Rules：先是标准 DataFile 固定顺序
   `Actors.json → Animations.json → Armors.json → Classes.json → CommonEvents.json → Enemies.json → Items.json → MapInfos.json → Skills.json → States.json → System.json → Tilesets.json → Troops.json → Weapons.json`；
   再是自定义 DataFile 按精确 UTF-8 基名字典序；再是 MapId 数值升序；最后是
   `plugins.js` 按插件数组 index、同一插件内按 `parameters` 对象成员的来源顺序。每个来源
   内按 JSON 对象成员来源顺序、数组数值下标、嵌套结构路径和同 string 捕获起始字节排列；
   规则编号和 OS 目录枚举顺序均不参与；
4. Lua：`groups` 数组、随后每组 `fields` 数组。

并发读取和计算只改变完成时间。顺序进入资产快照指纹，但不进入语义身份，也不单独阻止
原文、角色和上下文相同的译文继承。Reader、Planner 和 WriteBack 不再按角色字符串或
位置显示文本重新排序。

## 6. Mutation Claim 与冲突

每个 recipe 派生 Value、NoteTag、CommentTag 或事件块的物理 Claim，并展开成 Intent 与
Exclusive 资源锁。同一资源只允许 Intent+Intent；任一 Exclusive 即冲突。组内、同 owner、
跨 owner Store 与 WriteBack 发布前共用这一验证。

完整冲突矩阵见[规则文件第 5 节](rules.md#5-自然顺序与-mutation-claim)。关键结果是：
raw JSON string 与 decoded descendant 冲突，不同 decoded sibling 允许；raw note 与
NoteTag 冲突而不同 occurrence 允许；raw 108/408 与 CommentTag 冲突；Dialogue、Choices、
ScrollingText 与其覆盖字段或 descendant 冲突。

## 7. Lua 与三阶段停止线

Extract Lua 获得公共 `ctx.project/json/source/rpg_maker/db` 和 `ctx.extract`。简单的
“一个标量语义字段 → 一个受信物理文本位置”可用 `replace_standard` 接入 Standard。
跨文档、多目标复杂插件由 Lua 自己拥有 Extract/Translate/WriteBack 三阶段身份、私有表、
事务和幂等协议；核心不提供通用多目标 DSL 或发布后回调。完整示例见
[Lua Cookbook](lua-cookbook.md)。

## 8. 提交、继承与完成语义

每个 owner 的来源指纹、group、unit、claim 和资产快照指纹在一个事务中验证并替换。
失败不暴露半快照。相同快照经完整读取复核后返回 `Unchanged`；真实替换返回更新摘要。

替换时，只有逻辑身份、unit role、完整源内容与源上下文逐字相同才继承译文/state；
`group_order`/`unit_order` 的单独变化不破坏继承，但会改变资产快照指纹。跨 owner 的 Claim
冲突在提交前失败。成功摘要统计逻辑组和语义单元，不把物理位置或资源锁计为单元。

提取成功证明候选满足 ATT 契约，不单独证明所有文本都玩家可见。作者仍应做正反样本、
未翻译 round-trip、翻译 round-trip 和游戏内抽查。
