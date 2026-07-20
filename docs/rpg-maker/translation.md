# RPG Maker 翻译现行规格

RPG Maker 领域当前只支持 MZ 与 MV；两者共用唯一的翻译 Profile、Prompt、资源、
Planner、LLM 执行和 SQLite 提交能力。域内版本只决定项目位置和审计中的 `engine`，
不会改变翻译协议。

## 1. 命令与资源选择

```text
att --config FILE mz translate --name NAME PROFILE_ID \
  [--terms TERMS_TOML] [--placeholders PLACEHOLDERS_TOML] [--lua SCRIPT_LUA]

att --config FILE mv translate --name NAME PROFILE_ID \
  [--terms TERMS_TOML] [--placeholders PLACEHOLDERS_TOML] [--lua SCRIPT_LUA]
```

术语或占位符文件被提供时，内容完整替换项目中的对应资源；省略时复用项目数据库的
canonical JSON。文件严格解析和资源解析全部成功后，Standard 才开始规划。Standard
结束后执行显式 Lua；Standard 的 `Complete`、`Partial` 和 `Unavailable` 都是正常业务
结果，技术错误才阻止后续阶段。

项目开启时必须确认冻结来源、每个活动 owner 的来源指纹和资产快照指纹。翻译只读取
`standard_text_group/unit`；译文按语义单元身份写回 `standard_text_unit`，不以物理
JSON 地址寻址。

## 2. Terminology TOML

如何从 MV/MZ 结构化字段和上下文提炼术语，而不把术语表做成字段全集镜像，见
[术语表制作指南](terminology.md)。

```toml
[[term]]
term = "魔法剣"
translation = "魔法剑"
```

需要完整替换默认 trigger 集合时，写作：

```toml
[[term]]
term = "魔法剣"
translation = "魔法剑"
triggers = ["魔法剣", "魔剣"]
```

权威空术语表使用另一份完整文件：

```toml
term = []
```

根必须显式声明 `term`。每项的 `term` 与 `translation` 必填、非空白且没有首尾空白。
`triggers` 缺席时固定等于 `[term]`；显式提供时必须非空，并完整替换默认集合。术语
原文全局唯一，trigger 也全局唯一；trigger 是区分大小写的字面子串，不是正则。

`term = []` 是权威空术语表。零字节、仅注释、未知字段、重复字段、空值和重复身份均
作为普通无效当前输入拒绝。契约不包含 scope、优先级、多译名或版本。

## 3. Placeholder TOML

如何区分提取目标与保护协议、选择 scope 并验证正反样本，见
[规则编写指南](rules.md#6-placeholder-rules)。

```toml
[[rule]]
pattern = '\\SE\[[^]]+\]'

[[rule]]
scopes = ["event_dialogue"]
pattern = '<name>(?<text>.*?)</name>'
```

清除自定义规则使用另一份完整文件：

```toml
rule = []
```

`pattern` 必填并使用本机 PCRE2 UTF/UCP；`scopes` 缺席表示全局。无 `text` 命名捕获时
保护完整匹配；存在时必须只有这一个命名捕获，仅捕获内容进入翻译，匹配外壳受保护。
完整匹配与捕获必须有序、位于原字符串内并对齐 UTF-8 边界，`text` 还必须完整位于
对应匹配内。不接受 `label`、`translate`、`"all"` 或其他命名捕获。

`scopes` 只接受以下当前值：

```text
database_entry
system
map
event_dialogue
event_choices
event_scrolling_text
event_command
plugin_parameter
```

列表中的值精确匹配，不存在别名或父级 scope；同一条规则内不能重复声明 scope。

自定义规则零命中合法。Builtin 与 Custom、Custom 与 Custom 的任何跨度重叠都使本单元
规划失败，不按顺序切割，也没有优先级。`rule = []` 只清除自定义规则；固定 RPG Maker
控制符保护继续生效。

三类人工编写的 TOML（Extract Rules、Terminology、Placeholder）都严格拒绝未知与重复字段，
在边界建立受信类型。术语和自定义占位符随后编码为内部 canonical JSON 持久化。

## 4. Profile、Prompt 与共享语言能力

配置从 `[[rpg_maker.translation_profiles]]` 精确选择 `PROFILE_ID`，再取得该 Profile
引用的公共 LLM Client。项目开启后从 metadata 取得规范 `LanguagePair`，精确读取：

```text
<prompts.root>/rpg_maker/<source>--<target>.md
```

Prompt 必须是普通 UTF-8 非空白文件，不做父语言、大小写、默认文件或目录首项回退。
Profile 拥有 planning、request、任务并发及所选 Client；Prompt 与源语言模块属于按项目
语言对解析的资源。Standard 与 Translate Lua 复用同一 Client、Prompt、语言对和语言
语义，Lua 不取得 Standard 的 planning、request 或任务并发策略。

Prompt 内容、语言策略、术语、占位符和相关翻译事实共同进入 translation state；这些
事实未变且语义单元仍 Current 时不请求模型。

## 5. 自然顺序、上下文与去重

Planner 按稳定 RPG Maker 来源顺序读取活动 owner，再按组与角色组织语义单元：

```text
DialogueSpeaker → DialogueBody
Choices
ScrollingText
Scalar
```

一个完整语义组永不跨 TaskBlock 拆分。`101 + 401*` 的全部正文、一个 `102` 的全部选项
以及 `105 + 405*` 的全部滚动文本分别形成一个单元；原物理行索引只属于写回 recipe。
五类单元可以进入同一 TaskBlock。并发准备只改变完成时间，不改变顺序、代表单元、任务
ID 或提交顺序。

Speaker 与 Body 不互相去重。Speaker 按完整源值全局精确复用；Body 的去重与 translation
state 包含完整有序正文和该组源 Speaker 原文上下文，因此同一句正文在不同说话人下
不是同一翻译事实。Choices 不跨 group 去重；ScrollingText 按完整有序行序列去重；
Scalar 按来源语义域与字段角色去重，例如数据库文件类型加字段键，不能把所有 `name`
混为一类。
recipe 中的 Literal/SpeakerSlot 外壳不属于翻译语义：仅外壳变化且语义内容与上下文
不变时可以继承译文。

去重只接受当前项目、当前语言对中完全相同的受信翻译输入；不 trim、不折叠大小写、
不做 Unicode 或换行边界模糊匹配。哈希只作索引，最终仍以类型相等确认。重复单元使用
自然顺序中最早的代表，其他传播目标不发送给模型；已有一个有效译文时直接复用，验收
后把完整译文原子传播到等价集合；多个有效译文互相冲突时显式失败。

## 6. 任务规划与模型协议

Planner 先保持最大相关组，再按 Profile 的完整 messages 字符预算切分 TaskBlock；单个
语义组超过预算时明确失败，不切碎逻辑原子。每个活动语义单元获得从 `1` 连续编号的
任务内 ID。TaskBlock 只保留任务身份、语言对、messages 和权威 ExpectedOutputs。容量
以最终 system message 与最小 user message 的实际字符数计算。

user message 使用最小 Markdown，只发送命中术语、活动 ID、自然语言角色、必要形状约束
和直接有用的无编号上下文。语言对、路径、owner、内部 kind、去重原因、空区域和内部
数据模型都不属于消息内容。已有或复用译文作为上下文时优先显示目标译文；没有译文时显示
源文；Speaker 可作为活动正文的语境，数据库名称可作为同一条目说明的语境。传播目标、
与活动单元无直接关系的虚项和纯虚组完全省略。术语只在本请求活动原文命中时出现，零命中
时不生成术语区。Prompt 由外部提供，本规格只要求 Prompt 作者明确：只翻译带 ID 内容、
无 ID 内容只作语境、ID 恰好返回一次、遵守当前返回 wire、区分自由断行与严格对齐、
精确保留 ATT token，并禁止解释文字。

同一 user message 可以直接混合五种角色，例如：

以下内容是人工构造的协议示例，不来自游戏材料或验证样本。

```markdown
术语：

- 星港 → 星港

## 对话

说话人 [1]（单行）：ミレア

正文 [2]（自由断行）：

> 潮風が強くなってきました。
> 灯台へ戻りましょう。

## 选项

选项 [3]（2 项，逐项对应）：

> 戻る
> 進む

## 滚动文本

滚动文本 [4]（2 行，逐行对应）：

> 航海記
> 第三夜

## 数据库文本

简介 [5]（自由断行）：

> 星を読む航海士。
> 古い灯台を守っている。
```

公共 LLM 根固定提交 `model`、`messages` 和 `stream=false`，并透传 Client 的其他受信
JSON parameters。网络重试只按 Profile 的明确延迟和 `Retry-After` 上限执行；协议失败
不伪装成网络故障。

Standard 使用 `max_in_flight_tasks = N` 个持续 HTTP 消费者执行已经物化的有序
TaskBlock。HTTP 完成后，结果立即进入命令私有 CPU 根进行无副作用的提交准备；原 HTTP
消费者随即补入新任务，不等待 CPU 准备、较早慢请求或顺序 finalizer。不同任务的准备
可以乱序完成，准备产物仍按计划 index 落入独立槽；没有合格译文的结果不建立数据库
提交产物。

顺序 finalizer 只按 `0..n` 消费槽位。每个需要写入译文的任务各自执行独立的
`BEGIN IMMEDIATE → COMMIT` 短事务，不合并任务，也不降低 SQLite 根配置建立的同步等级；
提交明确成功后才更新报告并写该任务的终态审计。没有数据库写入的正常结果同样按序
更新报告和审计。任务许可直到对应终态审计完成后才释放，因此活动 HTTP 执行不超过
`N`，已启动但尚未 finalization 的 HTTP 执行、CPU 准备、已准备等待和顺序最终化合计
不超过 `2N`。重试始终属于原 TaskBlock，同时占用原 HTTP 执行槽和窗口许可。

正常 `Complete`、`Partial` 与 `Unavailable` 都继续补位。HTTP 执行、CPU 提交准备或
其他技术错误以及合作取消都会停止领取新任务，但已启动任务全部排空；并存技术错误按
最小计划 index 选择主错误，只继续提交首个技术失败前可确认的连续成功前缀。若失败
发生在某任务数据库已经明确提交后的终态审计，该任务写入不回滚，后序成功仍标记为
未提交。取消排空中出现的技术错误优先于取消。并发只改变完成时间，不改变提交、报告
或终态审计顺序。

上述消息的合法响应可以是：

```json
{
  "1": ["米蕾娅"],
  "2": ["海风越来越强了。", "我们回灯塔吧。"],
  "3": ["返回", "前进"],
  "4": ["航海记", "第三夜"],
  "5": ["能够解读星象的航海士。", "守护着一座古老的灯塔。"]
}
```

响应只接受一个可完整消费的 ID 到字符串数组的 JSON 对象。允许首尾空白、至多一个
开头 BOM，以及恰好包裹全部 JSON 的单层无标记或 `json` 围栏；不搜索说明文字，不修复
JSON、ID 或译文。顶层只能包含任务 ID；key 必须是规范正十进制形式，每个值都必须是
字符串数组。

权威输出形状只有 `Reflow` 与 `Aligned(N)`，由 ATT 根据角色建立，模型不能返回或修改。
DialogueBody 使用 `Reflow`；允许自由断行的 Scalar 仅为 `Actors.profile`、
`Skills.description`、`Items.description`、`Weapons.description` 与
`Armors.description`。Speaker、姓名和其他 Scalar 使用 `Aligned(1)`；Choices 与
ScrollingText 使用 `Aligned(N)`。数组元素不得包含 CR、LF 或 NUL。`Reflow` 允许显式
空行但不能是空数组或全空白；`Aligned` 的非空源槽不得返回空白，源空槽必须返回空字符串。
验收后，源内容为 Value 的 Reflow 结果以 `\n` 连接为一个 Value；源内容为 Lines 的结果
保持数组元素边界。

JSON 语法、顶层类型或协议外信封错误使整批
`Unavailable(ModelResponseUnusable)`。某个预期 ID 缺失、重复，或其值类型、行数、行内容、
自然语言、源文残留或占位符验收失败时，该预期输出保持未解决；其他合格输出仍可形成
`Partial`。响应中未知或非法的额外键只产生协议诊断并被忽略；只要全部预期 ID 都合格，
任务仍可为 `Complete`。`Reflow` 对完整连接后的语义文本执行 token、语言与占位符验收，
允许 token 跨原物理行移动；`Aligned` 逐槽验收，禁止 token 跨槽移动，包括从一个选项
移动到另一个选项。一个严格对齐 ID 整体成功或整体拒绝。

每个 ID 只产生一个原子 Translation Decision；同一任务的来源、状态条件或任一传播目标
在提交前发生漂移时，整笔任务事务回滚，不留下部分传播。

## 7. 原子验收与持久化

每个 TaskBlock 在发送第一次请求前写 `translation_task_started`。发送任何请求前，
Result Store 的 Planner 准备事务一次确认：

- metadata 来源指纹；
- 所有 owner 的来源与资产快照指纹；
- 本次术语和占位符 canonical JSON；
- 每个需要失效或复用的语义单元之 owner、group、role、完整源内容、上下文，以及预期旧译文与
  state 同时精确匹配或预期二者均为空。

上述只读条件按自然顺序批量校验，失效清理、复用写入与资源更新仍在同一准备事务中。
HTTP 验收结果随后由 Result Store 的 `prepare_commit` 在 CPU 根中校验重复单元、传播完整
内容与上下文并编码为只读提交产物，不做 SQLite I/O。顺序 finalizer 调用
`commit_prepared`，用一条条件 UPDATE 只 prepare 一次，再按自然顺序处理全部合格单元；
每组条件同时确认 owner、逻辑 group、role、完整源内容、上下文以及译文和 state 仍为空，
并且必须恰好修改一行。
任一条件失效都回滚该任务的整笔事务。

单个任务不会留下半提交；已经确认的前序任务不因后续 `Partial`、`Unavailable`、技术
错误或取消而回滚。任务终态审计记录 accepted、unresolved、协议诊断及
`confirmed_written_units`；位置均为逻辑组与单元角色，计数明确表示语义单元，而不是模型
返回行数或写回后的物理命令数。

## 8. Translate Lua 与完成结果

Translate Lua 获得公共 `ctx.project/json/source/rpg_maker/db`、阶段专属
`ctx.translation` 与 `ctx.llm`；`extract`、`output`、`write_back` 为 nil。
`ctx.translation.prepare/accept` 复用 Standard 的术语、占位符、语言分析与验收语义，
`ctx.llm` 复用本次 Client。可信脚本仍可通过 `ctx.db` 实现游戏专用协议；RPG Maker
文档能力的唯一门面是 `ctx.rpg_maker`。

成功输出报告任务、写入和剩余语义单元摘要；`Partial` 或 `Unavailable` 不冒充全部翻译
完成，也不升级为退出码 1。合作取消完成受控收尾后返回 130；配置、输入、技术、审计
或 shutdown 失败返回 1。
