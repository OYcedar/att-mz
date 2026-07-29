# RPG Maker 翻译现行规格

本文定义 MV/MZ Standard 与 Lua 可采用的 Managed 翻译能力之资源生命周期、任务规划、
译文检查、state、提交与恢复，以及独立项目 Lua 如何复用相应状态所有者的规则检查人工
译文。Standard 是 RPG Maker 核心翻译路径；Managed 的模型与执行内核保持引擎无关，并由
Lua 负责声明身份和最终写回关系。二者不共享身份或译文，也不构成与 Lua 并列的三个流程。
Prompt 配置与资源、模型消息内容、响应信封、JSON wire、临时 ID、输出形状和 ATT token
协议由[Prompt 资源与模型协议现行规格](prompts.md)统一规定；本文直接使用响应解析器返回
的结构化结果，不重新定义协议。
术语字段由[术语现行规格](terminology.md)唯一规定，Placeholder 字段由
[规则文件](rules.md#6-placeholder-rules)唯一规定，Lua Translate API 见
[Lua 技术参考](lua.md#7-translatepreparecurrent-与-accept)，Managed API 见
[托管 translate/open](lua.md#72-ctxtranslationstranslateopen)，Standard 与 Managed 人工
提交 API 见[独立项目 Lua](lua.md#8-独立项目-lua人工译文验收与提交)。

## 1. 命令与阶段顺序

<!-- att-example: illustrative -->
```text
att --config FILE mz translate --name NAME [PROFILE_ID] \
  [--terms TERMS_TOML] [--placeholders PLACEHOLDERS_TOML] [--lua SCRIPT_LUA]

att --config FILE mv translate --name NAME [PROFILE_ID] \
  [--terms TERMS_TOML] [--placeholders PLACEHOLDERS_TOML] [--lua SCRIPT_LUA]
```

显式提供 `PROFILE_ID` 时精确选择并替换本次方案；省略时复用上次成功 Translate 保存的
Profile。项目尚无保存 Profile 时省略是输入错误；保存 ID 在当前配置中不存在时明确失败，
绝不选择其他 Profile。

提供术语或 Placeholder 文件时，先严格解析并完整替换对应 canonical 资源；省略参数复用
项目当前资源。空定义必须分别显式提供 `term = []` 或 `rule = []`。任一资源无效时不开始
Standard 请求。

非空 `--lua FILE` 读取并保存该阶段主程序正文、SHA-256 与无损解析路径；省略 `--lua`
复用 Translate 阶段已保存程序，零字节文件只清除该阶段程序并且本轮不执行。Standard
完成后才执行本轮选中的 Lua；Managed 只有在脚本显式调用 `ctx.translations.translate()`
时执行。Standard 的 `Complete`、`Partial`、`Unavailable` 是正常业务结果，不阻止 Lua；
技术错误阻止后续阶段。Standard 或 Managed 已确认提交的译文不会因随后 Lua 失败组合
回滚。Lua 私有数据库状态不因清除主程序而被猜测或删除。

保存的 Translate Lua 只包含主程序正文；`require`、`loadfile`、`dofile`、`io`、`os`
读取或执行的模块、文件与进程仍是运行时外部依赖，不随主程序进入快照。模块解析、路径和
非受管副作用见 [Lua 技术参考](lua.md#2-vm连接与-ctx)。

Profile 与 Lua 各自保留类型化来源。显式 Profile 配合省略 `--lua` 时，Profile 来源为
显式输入，Lua 来源仍为项目状态（项目尚无 Translate 方案时为产品行为）。终端摘要和
项目日志分别呈现这两个来源，不能把混合方案笼统标成全部显式或全部复用。

项目开启时验证冻结来源、活动 Standard owner 的资产快照指纹，以及 Managed owner 的
manifest 指纹。Standard 以标准语义单元身份读写，Managed 以 collection name + unit key
寻址；两者都不要求模型理解物理 JSON 地址。

## 2. Standard、Managed 与低级 Lua 共用的翻译能力

Profile 由显式或项目状态解析出的 ID 在 `[[rpg_maker.translation_profiles]]` 中精确
选择，再解析它引用的公共 LLM Client。语言对来自项目 metadata；Prompt 的说明语言、
资源选择、模板校验和 system message 装配由
[Prompt 资源与模型协议现行规格](prompts.md#2-配置locale-与资源选择)完整定义。Translate
在首次 LLM 请求前取得本轮已经校验的完整 Prompt 与响应协议；资源或模板无效时，不进入
任务执行。

Standard、Managed 与低级 Translate Lua 复用本轮解析出的 engine、语言对、语言模块、
同一份已经渲染的 Prompt 资源、Client、实际 Placeholder 和实际有序术语命中。最终
system Prompt 按执行路径分离：Standard 与低级 Lua 保持资源既有装配字节；Managed 在
相同资源内容后追加 Managed 模块固定提供的机器协议片段。Managed 进一步复用 ATT 的
planning、request、响应信封、并发、重试、验收、state、checkpoint 与任务记录；低级
`ctx.translation/ctx.llm/ctx.db` 仍由脚本自行定义消息格式并确认写入结果，不继承这些
Managed 行为。

Managed 的 prepared content 是引擎无关领域契约，不属于 Lua VM。本轮自动 Managed、
独立项目 Lua 的 Managed 人工候选，以及低级 `prepare_content` 对相同
`single/reflow/lines/items` 输入复用同一份 shape、空槽、Placeholder、控制字符和语言
验收结论。自动与人工 Managed 继续由 Managed 状态所有者负责 collection/unit identity、
全局去重、Current state 和 checkpoint；低级接口只返回私有 prepared/state，由 Lua 自己
负责 identity、LLM、事务和持久化。共享验收不把 Standard 的 `TextUnitContent`、行策略、
recipe 或传播规则改成 Managed shape。

MV 与 MZ 共享翻译流程，但 engine 是语义事实：两者 Builtin 控制符矩阵不同，state 也
绑定 engine。精确矩阵见[规则文件](rules.md#68-mvmz-builtin-控制符矩阵)。

## 3. NaturalText、术语和 Placeholder

Standard Extract 先建立 Group 与 Unit；Unit 是 Standard 翻译验收、Current 和全局去重
的最小单位。每个 Unit 原文随后执行 engine 对应的 Builtin Placeholder 和当前 Custom
Placeholder，得到
有序 opaque 段及 NaturalText 段。Placeholder 不拆 Unit、不分配持久 ID，也不改变
Extract recipe。术语只逐段扫描 NaturalText，不扫描 opaque 外壳，不跨
OpaqueBoundary 或 Lines 元素拼接。`Value` 中的 LF 是该值本身的内容，Placeholder 可以
按规则跨 LF 匹配；`Lines` 元素之间的拼接 LF 是语义槽边界，任何实际 opaque 保护跨度
都不得包含该边界。带 `text` 捕获的完整 wrapper 匹配可以横跨多个元素，但前后实际
opaque wrapper 必须各自留在单个元素内，元素边界只能位于仍可翻译的 `text` 中。

Custom Placeholder 的 `scopes` 只匹配八种 TextGroup kind。同 kind 的 Builtin、Rules
Unit 与主动调用 `translation.prepare(kind, ...)` 的 Lua 私有文本使用同一规则；异 kind
不使用。规则不选择 owner、文件路径、Extract Rule 或 Lua 脚本，也不负责 Lua 私有
grammar 的解析和验收。

Managed unit 在 Extract 时显式声明相同八种 `kind`，因此使用同一 engine Placeholder、
Custom Placeholder、语言模块和实际术语匹配。它不进入 Standard Group/Unit/recipe 或
Mutation Claim；`single`、`reflow`、`lines`、`items` 自己定义模型输出和验收原子。
`lines/items` 的完整数组整体占一个模型 ID 并原子验收、提交，元素不是独立持久单元。

自定义 Placeholder 文件的解析或编译错误在任何单元规划前拒绝整份资源；规则已经成功
编译后，某个单元发生保护跨度冲突、占用 `⟦ATT_` 保留前缀或无法安全替换为 ATT token，
只形成该单元
自己的 `planning-unresolved`。它不取得模型 ID、不产生 LLM attempt，也不发给模型；同轮
其他单元继续规划。该单元的旧译文不再 Current，并与本轮合法术语/Placeholder 资源一起
在请求模型前由 Preparation 原子失效和提交。

同一条术语的多个 trigger 命中多次也只输出一次；多个命中条目保持术语文件顺序。该有序
结果由 Prompt、Standard state 与 Lua `prepared.terms` 共用，不对 raw original 二次匹配。
术语中的 Markdown 字符由序列化器按字面转义。

去除 opaque 后没有任何非空白 NaturalText 时，单元是 `FullyProtected`；源语言模块判断无需翻译时是
`NonSourceLanguage`。二者均不请求模型。

## 4. 持久自然顺序、上下文与去重

### 4.1 Standard

Reader 只接受 Extract 已验证的顺序：owner 固定 Builtin、Rules、Lua；owner 内按连续
`group_order`；组内按连续 `unit_order`。Planner 不再按角色字符串、显示位置或完成时间
重新排序。并发可改变准备/响应完成时间，不能改变代表选择、任务 ID 或提交顺序。

一个语义组永不跨 TaskBlock 拆分。对话 Speaker/Body、完整 Choices、完整 ScrollingText
分别保持 Extract 的逻辑原子和 Lines 边界。

去重规则：

- Speaker 按完整源 Value 全局精确复用，不与 Body 去重；
- Body 包含完整有序 Lines 和该组源 Speaker 上下文；
- Choices 不跨 group 去重；
- ScrollingText 按完整有序 Lines 去重；
- Scalar 按来源语义域与字段角色去重，不把所有同名字段混为一类。

不 trim、不折叠大小写、不做 Unicode 或换行模糊匹配。重复集合使用自然顺序最早代表；
已有一个 Current 译文时复用，多个有效译文冲突时失败。recipe 的 Literal 外壳不是翻译
语义；仅外壳变化、而语义内容和上下文相同时可继承译文。去重键同时包含完整 Unit
原文、角色/语义域、必要上下文、保护后的文本和实际 Placeholder 有序绑定；不同完整原文
或不同实际保护契约不会误合族。只为本轮没有 Current 种子的活动代表分配临时模型 ID，
持久化层不为去重族另造业务 ID。

### 4.2 Managed

Managed 的全局自然顺序是 Extract 快照中的 collection 顺序，再是各 collection 的 unit
顺序；重排、插入或删除只改变自然序和 manifest，不改变 `name + key` 身份。全部
collection 共同形成一个 Managed 去重域，但它与 Standard 去重域严格独立：即使两边
原文和语义相同，也不互相选择代表、传播译文或共享 Current。

Managed 去重键包含完整 prepared 语义：engine、kind、shape、精确 original（包括数组
边界）、collection instruction、unit context、语言对、语言模块、实际 Managed Prompt/Client
语义、实际有序术语和 Placeholder 绑定。collection `name`、unit `key`、metadata 以及
声明顺序不进入键。不 trim、不折叠大小写、不做 Unicode 或换行模糊匹配；完整语义相同
时选择全局自然顺序最早 unit 为代表。

同一族已经存在唯一 Current 译文时直接传播，不请求模型；存在两个或更多互相冲突的
Current 译文时，在任何 Managed 请求发出前失败。单元变为 non-applicable 时，旧
translation/state 在 Preparation 事务中成对清除。metadata 或顺序变化可以保留译文；
instruction、kind、shape、original、context 或其他已绑定语义改变时，旧 state 不再
Current。

## 5. 任务规划与模型消息

### 5.1 Standard TaskBlock

Planner 先保持最大相关组，再按 Profile 的最终 user message Unicode 字符装箱目标切
TaskBlock。目标计算实际命中术语、活动 ID、标签、上下文、正文、Markdown 转义与换行；
system Prompt 随每个任务完整发送，但其字符数不参与装箱。加入下一个完整组会超过目标
时，只在组边界开始下一任务；单组生成的最终 user message 本身超过目标时，该组独立
成为一个任务，实际字符数允许超过目标，翻译继续。后续任务仍使用原目标，不拆组、不
拒绝规范内容，也不把该目标解释为 Provider 容量上限。活动单元从 1 连续编号。

装箱范围由实际文档语义边界决定：System、每个普通标准数据库文件、每个额外 data
文件、每张 Map 和每个 Plugin 分别形成范围；`CommonEvents.json` 按单个
CommonEvent、`Troops.json` 按单个 Troop 建立范围。每个范围独立装箱，TaskBlock
不跨范围；只有同一范围内自然相邻的完整组可以共享 TaskBlock。该边界同时固定
TaskBlock 分区、每个任务的术语并集和完整模型请求语义，并保持组内必要上下文、源
上下文、组顺序及单元顺序。Reader 顺序被其他范围打断后，相同范围键不会跨段重新合并。

Planner 把组内需要翻译的内容转换为
[Prompt 资源与模型协议现行规格](prompts.md#4-模型实际收到什么)定义的最小 Markdown
user message。消息包含哪些内容、临时 ID 如何表示以及形状标记如何选择，都由该协议
规定；Planner 只负责按本节的处理范围和自然顺序提供正确输入。已有或复用译文作为上下文时
优先提供目标文，没有时使用源文。

### 5.2 Managed TaskBlock

Managed Planner 按 collection、unit 自然顺序规划，只把需要模型的全局去重代表放入
TaskBlock。TaskBlock 不跨 collection；加入下一个完整 unit 会超过同一 Profile 的最终
user message Unicode 字符装箱目标时，在 unit 边界开始下一任务。单个完整 unit 超过
目标时独占任务，不拆数组、不拒绝规范内容，也不改变后续任务目标。

Managed Planner 按同一
[模型消息协议](prompts.md#4-模型实际收到什么)。collection 与 unit 的持久身份只保留在
ATT 已经校验的任务计划和最终任务记录映射中；模型消息允许包含哪些内容、数组 unit 如何
使用 ID，以及哪些内部信息不得发送，都由该协议统一规定。

公共 LLM 请求格式由 [Chat Completions 现行规格](../runtime/chat-completions.md)规定。
网络重试仅按所选 Client 的明确策略；模型协议失败不伪装成网络错误。

## 6. 模型协议之后的译文检查

Executor 按[Prompt 资源与模型协议现行规格](prompts.md#6-响应信封模式)只解析一次响应
信封与 JSON wire，返回 Thinking、Assistant JSON、按顺序排列的合法条目和每个 ID 的协议
诊断；译文检查与任务记录共用这份结果，不重新解析标签、JSON 或 ID。信封或 JSON 根失败
时，整个 TaskBlock 以 `ModelResponseUnusable` 不可用，且不作为网络错误重试；JSON 根
合法后，只有通过 JSON 和输出形状检查的 ID 才进入下述译文检查，其他 ID 保留各自的协议
诊断。

Thinking 只是解析结果中的可读说明，不进入 `TranslationTaskOutcome`、数据库、state、
普通项目日志、终端或诊断，也不参与译文判断。启用翻译任务记录时，记录功能直接使用
解析结果，不重新猜测响应正文。

Managed 对每个已经通过 JSON 和输出形状检查的 ID，继续检查自然语言内容、源语残留、
语言修复和 Placeholder。四种 shape 的输入要求、每个位置如何检查 Placeholder，以及
一个 unit 必须整体接受和提交的规则，由
[Lua 技术参考](lua.md#62-ctxtranslationsreplace)规定；模型实际使用的五种输入标记
由 [Prompt 规范](prompts.md#5-翻译要求与五种输入标记)规定。当前不能把多个 unit 组成
一个共同接受或拒绝的组。

Standard Reader 在建立已经校验的身份时，使用与 Extract 相同的结构规则同时验证 group kind、
role 和 Value/Lines：对话组只接受 Speaker/Body，选项组只接受 Choices，滚动文本组只接受
ScrollingText，其余组只接受 Scalar。Executor 检查译文时继续使用同一规则；已经校验的身份中
kind/role 若不一致属于内部不变量，候选自身的形状或字符不合规则只拒绝对应 ID。

- Speaker 和严格单行字段把一条协议候选映射为 Value；
- 允许自由断行的 Scalar `Value` 把多条协议候选按 LF 连接；
- Choices 和严格 ScrollingText 把协议数组按槽映射为 Lines；
- 数量、字符、非空与空槽一致性由 Prompt 协议按 ID 检查，不属于 Unit 身份检查；
- 裸 `<` 与 `>` 是普通文本字节，新模型候选、Current 复用、去重传播和人工候选都按
  完整 Unit 契约逐字验收；只有明确命中的 Builtin/Custom Placeholder 跨度受到保护。

Planner 在构造每个 `ExpectedTranslationOutput` 时一次性校验传播上下文数量、Value/Lines
形状、行数、受保护文本与占位符 multiset，并建立该输出唯一的 Placeholder binding
索引。Executor 只消费这份已验证契约和缓存索引，不在每次响应验收时重建索引或重新
校验相同的静态 Planner 事实。

候选随后执行：BOM/全空白检查、ATT token 数量与对齐检查、唯一原片段回显归一化、目标
自然语言检查、源语残留分析及当前语言模块允许的修复。失败仅拒绝相应候选并形成结构化
unresolved；不会伪造译文。全部 ID 通过时任务为 `Complete`；部分通过时为 `Partial`；
JSON 根有效但全部输出被拒绝时原因为 `AllOutputsRejected`。

候选缺少某个 token 时，只有对应原片段属于唯一槽且在候选中恰好回显一次，才可恢复为
该 token；多个同字节槽或多次回显无法唯一对应时，以
`placeholder_normalization_ambiguous` 拒绝。token 已经在场时，额外出现的 Builtin
原控制符仍由内建控制语义拒绝；Custom 原片段不会反向扫描候选正文，正文中的同字节内容
保持 NaturalText。已经验收并持久化的译文按下一节 Current 规则处理，不重新执行这项
反向正规化。

## 7. translation state 与 Current

state 是当前译文成立所依赖语义的 SHA-256 摘要。它绑定：

- engine、语言对、语言模块；
- 当前执行路径实际发送的完整 Prompt（由此确定响应信封）和影响模型语义的 Client 事实；
- kind、完整 original、Lines 边界与源上下文；
- 实际选中的 Placeholder 有序绑定；
- 实际有序术语命中；
- 最终已验收译文。

对 Managed，collection `instruction`、unit `kind/shape/original/context` 与数组边界进入
同一语义摘要；collection `name`、unit `key`、metadata 和自然顺序不进入。身份字段用于
精确读写，不能替代 state；metadata 只供 Lua 写回读取。

规则诊断编号、未命中术语/Placeholder、并发、重试、队列和完成时间不进入 state。
自定义 Placeholder 编号也不进入 token、label 或 state。因此插入、删除或重排未命中规则
不会重译；重排已命中术语会改变有序命中并失效。

项目中译文/state 成对存在，且重新计算的 state 精确相等时直接判为 Current：不重新请求
模型，也不把恢复后的旧译文再次送入候选正规化。这样重复相同占位符的合格译文第二次
运行为零 LLM。切换 Prompt locale、`thinking_output` 或修改本轮选择的 Prompt 资源都会
改变已绑定语义；任何已绑定语义变化后，旧译文不再 Current。

低级 Lua 暴露相同语义的 64 字符小写十六进制 state，但私有身份和成对事务由脚本负责。
标量 `prepare` 保持既有 state 字节；结构化 `prepare_content` 另外绑定
`single/reflow/lines/items`、完整标量或数组边界、semantic context 和规范内容，两个
接口的 state 不能互换。完整接口见
[Lua Translate API](lua.md#7-translatepreparecurrent-与-accept)。

## 8. 独立项目 Lua 的人工候选

独立 `mv|mz lua` 命令可在不请求 LLM 的情况下，分别打开 Standard 或 Managed 人工候选
会话。显式 `--profile` 精确选择本次 Profile；未显式指定时，首次打开任一会话才延迟复用
项目上次成功 Translate 保存的 Profile。一次 VM 中两个接口共用这次解析结果、项目当前
canonical 术语和 Placeholder，以及相同的语言与 Prompt/Client 语义；它们不接受临时资源
覆盖，也不替换保存方案。

### 8.1 Standard 人工候选

打开 Standard 会话时，核心从一致数据库快照读取完整物理单元、Value/Lines 边界、源
上下文、当前译文/state 和全部潜在去重成员，并用普通 Standard Planner 的全局语义指纹
建立只读单元。打开和枚举本身没有副作用：不会全局清除失效译文，也不会自动传播已有
Current。

人工候选继续经过普通 Standard 共用的 Placeholder 恢复、line shape、自然语言、源语
残留、语言修复和精确去重规则。Lua 只提交候选和是否允许替换 Current 的明确意图；它
不能读取、构造或写入 state。人工提交也没有永久特权：Profile、Prompt、Client、语言
模块、术语、Placeholder、原文或源上下文改变后，下一次 Planner 会按同一规则把旧结果
判为非 Current。

每批先验收全部候选。普通候选拒绝逐项返回且不写库；合法去重族在一个短 SQLite 事务中以
CAS 提交。事务内重新检查项目 source snapshot、owner/resource 指纹，以及每个传播位置的
完整身份、原文、源上下文和读取时 translation/state pair；任一目标陈旧则整批合法族
回滚。每个传播位置分别计算自己的正确 state，translation/state 始终成对。

### 8.2 Managed 人工候选

Managed 会话读取完整 source、owner/manifest、collection/unit、prepared content、当前
translation/state 和全部全局去重成员。只读 unit 按 collection、unit 自然顺序投影
`current`、`missing`、`stale`、`not_applicable` 或 `unavailable`；打开、查找和枚举不会
修改数据库。

候选复用自动 Managed 的四种 shape、Placeholder、语言和去重族规则。同批同族必须提供
相同候选与覆盖选项；已有 Current 与规范候选相同为幂等成功，并可补齐同族
missing/stale unit；改变任一 Current 成员必须显式允许替换。普通拒绝排除在写入外，全部
合法族在一个短事务中共同提交。

事务 CAS 再次检查打开会话时的完整冻结快照和每个成员旧 translation/state pair，任何
并发变化都让合法族整体回滚。确认未应用与提交结果未知保持不同技术终态；结果未知时不能
盲目重试。成功返回后该次提交已经生效，同一会话后续查询投影为新 Current，先前取得的
userdata 仍是旧只读投影。

两个接口在活动交互 `ctx.db` 事务中都拒绝提交。脚本后续失败或取消不会回滚已经成功返回
的人工批次。同一 Profile 再运行普通 Translate 时，人工提交且仍 Current 的族不产生 LLM
任务；Standard 按普通 recipe 写回，Managed 由 Lua 显式选择安全完整 Value 写回或私有
grammar。独立命令不生成 TaskBlock 或任务记录，也不修改原游戏目录。完整 Lua 表面和
冲突规则见[Lua 技术参考](lua.md#8-独立项目-lua人工译文验收与提交)。

## 9. 并发、提交与任务结果

所选 Client 的 `max_concurrent_requests`（记为 N）决定同时进行的 HTTP 请求数。完整
Corpus、Plan、Task 和传播目标可以保存在内存。Standard 与 Managed 使用同一个有序执行器：
网络请求和不修改项目的准备工作可以乱序完成；一个响应完成后立即释放 HTTP 名额；数据库
写入仍严格按任务的自然顺序进行。活动请求、已经完成但等待前序任务的结果，以及正在写入
数据库的任务合计最多 3N 个。这个窗口固定在程序中，不属于 Profile 或 Lua 配置。

执行器统一处理 Client 重试次数、`Retry-After`、合作取消和已经开始的请求。发生技术错误
后不再启动新任务；已经确认写入的自然顺序前缀保持不变。取消后同样不再启动新任务，但
每个已经发出 `TaskStarted` 的任务仍会得到明确的“已写入、未写入或已取消”结果；尚未启动
的任务不产生任务记录。确认取消后，已经通过模型响应检查但尚未开始写入的任务不再进入
SQLite。ATT 不猜测外部请求是否发生。

Standard 与 Managed 各自维护任务计划和数据库适配器。Standard 为每个需要写入的任务
使用独立 SQLite 事务，并在同一事务中传播已经通过检查的重复内容。Managed 在发出请求前，
从同一数据库快照完成 Current 检查、non-applicable 清理、全局去重和 Current 冲突检查；
随后每个 TaskBlock 用一次短 CAS checkpoint 写入全部已经接受的 translation/state 对。
CAS 同时核对 owner 来源、manifest、unit 身份以及读取时的 translation/state pair。
`NotApplied` 表示这次 checkpoint 没有修改项目；`OutcomeUnknown` 表示无法确认是否写入，
此时立即停止启动和提交后续任务，不能把它显示成普通的“未写入”。

普通不可用结果保持 translation/state 为空并允许下次重试。JSON 根合法时，合法 ID 可以
与同任务的拒绝 ID 一起形成部分 checkpoint；数组 unit 自身仍全部接受或全部不写。技术
失败或取消只保留已经确认提交的自然序前缀。

Standard 最终结果仍是：全部需要翻译的单元 Current 为 `Complete`；仍有可解释 unresolved
为 `Partial`；没有可执行产出为 `Unavailable`。Managed `translate()` 通过逐 unit
`current/translated/not_applicable/unavailable` 报告相同事实。它们是业务结果，退出成功
或 Lua 调用正常返回不等于每个单元都有译文；技术错误、状态不一致或提交结果未知使用
更强失败语义。

模型响应拒绝作为 task unresolved 记录；每个译前 Placeholder 处理失败的单元保留独立的
结构化诊断信息，包含逻辑位置和专用原因，不伪造 task ID、模型 attempt 或响应拒绝原因。
两者都计入最终 `remaining_decisions` 与 `remaining_locations`；因此即使所有已发送任务
都完成，只要仍有 planning-unresolved，本轮也不能解释为全部 Current。普通项目日志可
按稳定 code 和结构化字段记录摘要，但日志缺失不改变这些结果或数据库写入状态。

Standard 或 Managed 建立真实任务总数后，进度只在对应任务的数据库写入结果确认后推进；业务
Complete、Partial 与 Unavailable 都算已确认。零任务显示“无需调用模型”，不显示
`0/0`。到达计划总数后仍进入必要收尾与保存运行方案。

全部翻译工作和必要的资源清理成功后，最后一个短事务精确替换
`translate_run_plan` 及 Translate Lua 程序。确认提交失败时旧方案保持；写入结果无法确认时
命令说明翻译结果已生效但方案状态无法确认，并建议下次显式传入 Profile 与 Lua 选择。
项目日志的启动、写入或关闭故障不停止模型任务、不丢弃合法候选，也不改变退出码。

## 10. Standard 与 Managed 任务记录

Translate 可以按配置生成 Standard 与 Managed 任务记录。哪些任务会被记录、文件如何
编号、保存哪些请求和结果、写入失败是否影响翻译，以及低级 `ctx.llm` 是否生成记录，
统一由[翻译任务记录现行规格](task-records.md)规定；本文不重复这些规则。
