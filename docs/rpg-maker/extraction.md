# RPG Maker 文本提取现行规格

本文定义 MV/MZ Extract 的生命周期、标准资产、Builtin 覆盖、顺序与事务。三类声明式
文件的字段和错误由[规则文件现行规格](rules.md)唯一规定；Lua 形状由
[Lua 技术参考](lua.md)规定。

## 1. 命令与 owner 生命周期

<!-- att-example: illustrative -->
```text
att --config FILE mz extract --name NAME \
  [--builtin] [--rules RULES_TOML] [--lua SCRIPT_LUA]

att --config FILE mv extract --name NAME \
  [--builtin] [--rules RULES_TOML] [--lua SCRIPT_LUA] \
  [--dialogue-rules DIALOGUE_TOML]
```

未提供 `--builtin/--rules/--lua` 时，命令复用项目上次成功 Extract 保存的完整 owner 方案；
项目尚无方案时明确要求至少提供一个提取选项。只要显式提供任一 owner，本轮显式集合就
精确替换自动方案：未列出的 owner 不执行，但其既有资产不会仅因未列出而删除。执行和
owner 总顺序始终是 `Builtin → Rules → Lua`。三个实际执行的 owner 分别原子替换自己的
快照；首个技术失败阻止后续 owner，此前已成功提交的 owner 不做组合回滚，但本次保存
方案不会替换旧方案。

`--dialogue-rules` 只属于 MV 且必须同时选择 `--builtin`。提供文件完整替换姓名定义，
省略时复用项目定义，`rule = []` 明确清空。定义与 Builtin 快照同事务提交。

- 非空 `--rules FILE` 在读取、解析、编译成功后保存已验证的 canonical 语义；自动复用
  直接执行该语义，不重新读取原 TOML 路径；
- `rule = []` 停用 Rules owner、删除其标准资产，并把 Rules 移出后续自动方案；
- 非空 `--lua FILE` 保存主程序正文、SHA-256 和无损解析路径；自动复用执行保存的正文；
- 零字节 Extract Lua 文件不执行程序，而是停用 Lua owner、删除其标准资产并清除该阶段
  程序；
- 清除后若没有任何可执行 owner，则删除保存的 Extract 方案；下次无参数运行会得到
  “尚无可复用方案”的输入错误。

Lua 保存路径只用于 chunk 名、`require` 搜索目录和诊断。主程序主动加载的模块、文件与
进程仍是可信 Lua 的动态外部依赖，不纳入快照。WriteBack 只消费已物化 recipe，不重读
TOML 或正则。

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

组类型、角色和内容形状作为同一个领域结构一次验证：`event_dialogue` 只接受
DialogueSpeaker/DialogueBody，`event_choices` 只接受 Choices，
`event_scrolling_text` 只接受 ScrollingText，其余组类型只接受 Scalar。Scalar Value
保留内部 LF；DialogueSpeaker Value 拒绝 CR/LF；所有 Value 拒绝 NUL。Lines 的元素边界
属于翻译事实，每个元素都拒绝 CR、LF 或 NUL。纯空白、Lines 数量对齐和空槽对应关系仍由
实际消费这些语义的 Extract、Translate 或 WriteBack 边界分别判断，不复制结构规则。

`source_context` 当前由 DialogueBody 保存源 Speaker，其余为空对象。译文内容必须与原文
形状相同，译文与 translation state 必须成对存在或成对为空。Lua 私有表使用的 64 字符
十六进制表示见 [Lua Translate](lua.md#7-translatepreparecurrent-与-accept)。

当前数据库相关表为：

- `standard_asset_owner_state`：owner 的来源与资产快照指纹；
- `standard_text_group`：含 `group_order` 的组与完整 recipe；
- `standard_text_unit`：含 `unit_order` 的语义单元、译文和 state；
- `standard_mutation_claim`：每个 `(owner, resource)` 至多一行的确定性跨 owner 冲突摘要；
- `standard_translation_resource`：术语与自定义占位符 canonical 资源；
- `standard_project_definition`：MV 姓名投影定义。
- `extract_run_plan`：上次成功 Extract 的非空完整 owner 集合；
- `extract_rules_definition`：可自动复用的非空 Rules canonical 语义；
- `lua_program` 中的 `extract` 行：可自动复用的非空 Lua 主程序快照。

其中 group location 与 Mutation resource 只使用当前完整 Value 地址的 compact canonical
JSON；事件块 Claim 由多个 Value 地址组成。unit role 和 recipe 也只使用当前 canonical
字节；含额外空白、替代转义或其他语义等价但非规范的表示时，按普通无效项目状态处理。

数据库只按当前 schema、约束和领域不变量读取；不符合时按具体 schema、状态或完整性
错误处理。

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

标准消息块是 `101 + 连续 401*`。MZ 从可选 `101.parameters[4]` 建立原生 Speaker；参数
缺失、空 string 或全空白 string 表示没有 Speaker，显式 `null` 或其他非 string 值是无效
源文档，不按“没有 Speaker”跳过。MV 按项目当前姓名投影处理第一条 `401`，精确语义见
[规则文件](rules.md#3-mv-对话姓名投影)。全部正文形成一个 DialogueBody，正文中的空白
`401` 作为 Lines 空元素保留；全空正文不建立 Body。

一个 `102` 的完整选项数组形成一个 Choices，包括空槽；recipe 同时声明对应同 indent
的 `402.parameters[1]` 和终止 `404`。事件列表只按自然命令顺序前向扫描一次；扫描期间
按 indent 维护尚未遇到同层 `404` 的非空 Choices，嵌套块不会重新遍历父区间。Choices
仍按其 `102` 的位置进入自然顺序，分支正文和嵌套 Choices 保持各自原始命令顺序。空数组
或全部为空白的选项数组不建立组，也不要求为被跳过的空组补造 `402/404`。

滚动文本按 `105 + 连续 405*` 建组，全部 `405` 形成一个 ScrollingText，包括空行。
`320/324/325` 只取参数 1。

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
`group.location` 本身必须唯一，同位置同 kind 或不同 kind 都直接失败，不会交给共享归一化
自动合并。

Group 是保持上下文、自然顺序、投影 recipe 和共同修改范围的最小单元组；Unit 才是候选
验收、Current 判断和全局去重的最小翻译单元。Placeholder 在 Unit 已经形成后运行，只把
Unit 内部划分为 NaturalText 与 opaque 段；它不拆 Unit、不建立持久身份，也不改变
Extract 物化的 recipe。

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

每个 recipe 派生完整 Value 或事件块的物理 Claim，并展开成 Intent 与 Exclusive 资源锁。
同一资源只允许 Intent+Intent；任一 Exclusive 即冲突。组内、同 owner、跨 owner Store
与 WriteBack 发布前共用这一验证。

这里的 Claim 是完整逻辑集合，由 group kind、location 和 recipe 唯一确定，并全部进入
owner 资产指纹。SQLite 不逐条复制这个集合：`standard_mutation_claim` 对每个
`(owner, resource)` 只保存跨 owner 冲突所需的确定性摘要。Exclusive 在 owner 内本来就
只能唯一，直接保留；多个合法 Intent 共享同一 resource 时，保留自然顺序最早的 group
作为代表。摘要不减少组内或 owner 内验证，也不改变完整逻辑 Claim 的指纹顺序。

完整冲突矩阵见[规则文件第 5 节](rules.md#5-自然顺序与-mutation-claim)。关键结果是：
raw JSON string 与 decoded descendant 冲突，不同 decoded sibling 允许；Dialogue、
Choices、ScrollingText 与其覆盖字段或 descendant 冲突。Value 中出现 `<`、`>` 或任何
插件私有语法都不改变 Claim 身份和冲突关系。

## 7. Lua 与三阶段停止线

Extract Lua 获得公共 `ctx.project/json/source/rpg_maker/db` 和 `ctx.extract`。简单的
“一个标量语义字段 → 一个受信物理文本位置”可用 `replace_standard` 接入 Standard。
跨文档、多目标复杂插件由 Lua 自己拥有 Extract/Translate/WriteBack 三阶段身份、私有表、
事务和幂等协议；核心不提供通用多目标 DSL 或发布后回调。完整示例见
[Lua Cookbook](lua-cookbook.md)。

## 8. 提交、继承与完成语义

每个 owner 的来源指纹、group、unit、由 recipe 确定的完整逻辑 Claim、冲突摘要和资产
快照指纹在一个事务中验证并替换。事务直接批量写正式 Group、Unit 与 Claim 摘要表；
未提交行对其他连接不可见，不复制 TEMP B-tree。非空 incoming 摘要不少于其他两个
owner 的摘要总量时，按最大真实游戏消融结果在同一替换事务内暂时删除两个 Claim
二级索引，直接写正式摘要表后用项目数据库的权威 DDL 恢复索引，再执行精确跨 owner
冲突检查；较小 owner 继续在线维护索引。这只是内部写入算法选择，不限制任一 owner
或项目的完整逻辑 Claim 总量，也不改变最终 schema。
指纹、无变化判断和译文继承完成后，事务参数可以按当前 B-tree 的物理键重排；持久化的
`group_order`、`unit_order` 与预先计算的指纹仍是自然语义，读取结果不随写入顺序改变。
跨 owner 检查以 `incoming_summary_count + 1` 为上限采样另一侧，再由实际较小的一侧驱动
精确资源索引探测。冲突、SQLite 错误或取消会把旧资产行和索引 DDL 一并回滚，不暴露
半快照；只有 SQLite 已确认回滚时才报告普通 Claim 冲突。相同快照经 group、unit 和摘要
的完整读取复核后返回 `Unchanged`；真实替换返回更新摘要。

替换时，只有逻辑身份、unit role、完整源内容与源上下文逐字相同才继承译文/state；
`group_order`/`unit_order` 的单独变化不破坏继承，但会改变资产快照指纹。跨 owner 的 Claim
冲突在提交前失败。成功摘要统计逻辑组和语义单元，不把物理位置或资源锁计为单元。

提取成功证明候选满足 ATT 契约，不单独证明所有文本都玩家可见。作者仍应做正反样本、
未翻译 round-trip、翻译 round-trip 和游戏内抽查。

只有全部所选 owner 成功且必要非日志根完成收尾后，命令才在最后一个短事务中精确替换
`extract_run_plan`、Rules 定义和 Extract Lua 程序。事务确认失败时旧方案保持；提交终态
无法确认时明确报告业务结果与方案状态无法确认。项目日志失败不会停止 owner、回滚合法
快照或改变退出码。

实时进度先显示 owner 阶段 `i/N`；文档、Builtin 工作单元和 Rules 规则只有在真实分母
建立后才显示局部计数，Lua 与 SQLite 提交使用 spinner。到达局部 `N/N` 后仍会显示收尾
和保存运行方案，不能提前解释为命令成功。
