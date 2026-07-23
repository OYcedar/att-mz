# RPG Maker 翻译现行规格

本文定义 MV/MZ Standard Translate 的资源生命周期、规划、模型协议、验收、state 与提交。
术语字段由[术语现行规格](terminology.md)唯一规定，Placeholder 字段由
[规则文件](rules.md#6-placeholder-rules)唯一规定，Lua Translate API 见
[Lua 技术参考](lua.md#7-translatepreparecurrent-与-accept)。

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
完成后才执行本轮选中的 Lua。Standard 的 `Complete`、`Partial`、`Unavailable` 是正常
业务结果，不阻止 Lua；技术错误阻止后续阶段。Standard 已提交译文不会因 Lua 失败组合
回滚。Lua 私有数据库状态不因清除主程序而被猜测或删除。

Profile 与 Lua 各自保留类型化来源。显式 Profile 配合省略 `--lua` 时，Profile 来源为
显式输入，Lua 来源仍为项目状态（项目尚无 Translate 方案时为产品行为）。终端摘要和
项目日志分别呈现这两个来源，不能把混合方案笼统标成全部显式或全部复用。

项目开启时验证冻结来源、活动 owner 来源指纹和资产快照指纹。翻译以标准语义单元身份
读写，不以物理 JSON 地址寻址。

## 2. 公共翻译语义

Profile 由显式或项目状态解析出的 ID 在 `[[rpg_maker.translation_profiles]]` 中精确
选择，再解析它引用的公共 LLM Client。语言对来自项目 metadata；Prompt 的说明语言由
`[prompts].locale` 独立选择。Translate 要求配置完整提供：

```toml
[prompts]
root = "prompts"
locale = "auto"
thinking_output = false
```

`locale = "auto"` 复用本进程已经解析的有效 UI locale；显式 locale 覆盖它。规范 locale
精确选择 `<prompts.root>/rpg_maker/<locale>/system.md`。该模板只允许并且必须包含
`{{source_language}}` 与 `{{target_language}}`，ATT 用项目规范 `LanguageId` 替换。只有
`thinking_output = true` 时才读取同目录 `thinking.md`，并以两个 LF 追加到渲染后的
system Prompt。

被读取的资源必须是普通 UTF-8 非空白文件，模板必须完整有效；任何错误都在首次 LLM
请求前失败。资源只按所选 locale 的精确路径读取。每次 Translate 都重新读取所选资源；
关闭模式不读取 `thinking.md`。完整契约见
[系统提示词编写指南](prompts.md)。

Standard 与 Translate Lua 复用本轮解析出的 engine、语言对、语言模块、已装配 Prompt、
Client、实际 Placeholder 和实际有序术语命中。Lua 不继承 Standard 的 planning、request、
响应信封验收或任务并发策略。

MV 与 MZ 共享翻译流程，但 engine 是语义事实：两者 Builtin 控制符矩阵不同，state 也
绑定 engine。精确矩阵见[规则文件](rules.md#68-mvmz-builtin-控制符矩阵)。

## 3. NaturalText、术语和 Placeholder

每个原文先执行 engine 对应的 Builtin Placeholder 和当前 Custom Placeholder，得到有序
opaque 段及 NaturalText 段。术语只逐段扫描 NaturalText，不扫描 opaque 外壳，不跨
OpaqueBoundary 或 Lines 元素拼接。

自定义 Placeholder 文件的解析或编译错误在任何单元规划前拒绝整份资源；规则已经成功
编译后，某个单元发生保护跨度冲突、占用 `⟦ATT_` 保留前缀或无法安全投影，只形成该单元
自己的 `planning-unresolved`。它不取得模型 ID、不产生 LLM attempt，也不发给模型；同轮
其他单元继续规划。该单元的旧译文不再 Current，并与本轮合法术语/Placeholder 资源一起
在请求模型前由 Preparation 原子失效和提交。

同一条术语的多个 trigger 命中多次也只输出一次；多个命中条目保持术语文件顺序。该有序
结果由 Prompt、Standard state 与 Lua `prepared.terms` 共用，不对 raw original 二次匹配。
术语中的 Markdown 字符由序列化器按字面转义。

去除 opaque 后没有任何非空白 NaturalText 时，单元是 `FullyProtected`；源语言模块判断无需翻译时是
`NonSourceLanguage`。二者均不请求模型。

## 4. 持久自然顺序、上下文与去重

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
语义；仅外壳变化、而语义内容和上下文相同时可继承译文。

## 5. 任务规划与模型消息

Planner 先保持最大相关组，再按 Profile 的最终 messages 字符预算切 TaskBlock。单组超过
预算时明确失败，不切碎。这里的预算包含按 locale 渲染并按思考模式装配后的完整 system
Prompt。活动单元从 1 连续编号。

user message 是最小 Markdown 载荷，只包含实际命中术语、活动 ID、自然语言角色、必要
形状约束和直接有用的无编号上下文。owner、路径、内部 kind、传播目标、去重原因和空区
不发送。已有/复用译文作上下文时优先目标文，没有时用源文。

<!-- att-example: illustrative -->
```markdown
Terminology:

- 星港 → 星港

## Dialogue

Speaker [1] (single line):ミレア

Body [2] (free line breaking):

> 潮風が強くなってきました。
> 灯台へ戻りましょう。

## Choices

Choices [3] (2 items, corresponding item by item):

> 戻る
> 進む
```

Prompt 作者必须遵守[系统提示词编写指南](prompts.md)定义的输入解释、模板变量、JSON
wire、ID、行形状、空槽、ATT token 和响应信封约束。Planner 与 Executor 共享已解析的
`TranslationResponseEnvelope`：关闭思考输出为 `JsonOnly`，开启为 `ThinkingThenJson`，
避免 Prompt 与解析器使用不同模式。违反这些约束会使整个 TaskBlock 或对应 ID 不可用，
不是单纯降低翻译质量。

公共 LLM 根提交 `model`、`messages`、`stream=false` 并透传 Client 的受信 parameters。
网络重试仅按所选 Client 的明确策略；协议失败不伪装成网络错误。

## 6. 响应结构与候选验收

`JsonOnly` 要求 assistant content 直接提供 JSON，任何 `<why>` 都非法。
`ThinkingThenJson` 要求整个 TaskBlock 精确输出一组
`<why>非空任意内容</why>`，随后只允许空白再接 JSON。标签必须精确小写且无属性；缺失、
空、未闭合、嵌套、重复、大小写变体、属性或前置说明都会拒绝。ATT 验证非空后立即丢弃
思考正文，不将其放入结果、数据库、普通项目日志、终端或诊断，也不判断分析质量。

响应整体允许首尾空白和最开头的单个 BOM；BOM 必须位于裸 JSON 或 `<why>` 之前，不能
放在 `</why>` 与 JSON 之间。剥离信封后的部分继续进入唯一的既有 JSON parser，并保留
唯一一层独占行 JSON 围栏容错；Prompt 仍必须要求裸 JSON。信封或 JSON 根失败使整个任务
因 `ModelResponseUnusable` 不可用，不作为网络错误重试；逐 ID 规则没有任何变化。

Standard 响应必须提供当前 TaskBlock 的每个 ID 恰好一次，不能缺失、重复或增加 ID。
Value/Lines 形状必须符合角色：

- Speaker 和形状为 `single line` 的严格字段是一条非空候选；
- `free line breaking` Value 可由多模型行组成，最终以 LF 连接；
- Choices 和严格 ScrollingText 必须与源 Lines 按槽对齐并保持空槽；
- 每个结构行拒绝 CR、LF、NUL 和非法空白形状。

候选随后执行：BOM/全空白检查、ATT token 数量与对齐检查、占位符无歧义恢复、目标自然
语言检查、源语残留分析及当前语言模块允许的修复。失败仅拒绝相应候选并形成结构化
unresolved；不会伪造译文。全部 ID 通过时任务为 `Complete`；部分通过时为 `Partial`；
JSON 根有效但全部输出被拒绝时原因为 `AllOutputsRejected`。

原文中相同占位符出现多次时，每个源位置仍是独立槽。**新**模型响应若无法无歧义恢复，
以 `placeholder_normalization_ambiguous` 拒绝。已经验收并持久化的译文按下一节 Current
规则处理，不重新执行这项反向正规化。

## 7. translation state 与 Current

state 是当前译文成立所依赖语义的 SHA-256 摘要。它绑定：

- engine、语言对、语言模块；
- 完整装配后的公共 Prompt（由此确定响应信封）和影响模型语义的 Client 事实；
- kind、完整 original、Lines 边界与源上下文；
- 实际选中的 Placeholder 有序绑定；
- 实际有序术语命中；
- 最终已验收译文。

规则诊断编号、未命中术语/Placeholder、并发、重试、队列和完成时间不进入 state。
自定义 Placeholder 编号也不进入 token、label 或 state。因此插入、删除或重排未命中规则
不会重译；重排已命中术语会改变有序命中并失效。

项目中译文/state 成对存在，且重新计算的 state 精确相等时直接判为 Current：不重新请求
模型，也不把恢复后的旧译文再次送入候选正规化。这样重复相同占位符的合格译文第二次
运行为零 LLM。切换 Prompt locale、`thinking_output` 或修改本轮选择的 Prompt 资源都会
改变已绑定语义；任何已绑定语义变化后，旧译文不再 Current。

Lua 暴露相同语义的 64 字符小写十六进制 state，但私有身份和成对事务由脚本负责，见
[Lua Translate API](lua.md#7-translatepreparecurrent-与-accept)。

## 8. 并发、提交与任务结果

所选 Client 的 `max_concurrent_requests`（记为 N）决定活动 HTTP 数。完整 Corpus、Plan、
Task 和传播目标可以保存在内存；调度器区分 HTTP 许可与顺序提交窗口，响应完成后立即
释放网络许可。任务结果仍按计划 index 稳定归并，顺序 finalizer 只按 `0..n` 提交。
SSPV 的 Release/MSVC 消融与慢首任务压力测试共同选定 2N 完成窗口，因此本地最多保留
3N 个已经入场但尚未顺序最终化的任务；该内部值固化在代码中，不进入 Profile 配置。

每个有写入的任务使用独立 SQLite 事务，验收通过的重复集合在同事务传播。技术失败停止
后续工作；已经提交任务保持。取消停止新请求，等待已接管工作到明确终态，不猜测外部
请求是否未发生。

最终：全部需要翻译的单元 Current 为 `Complete`；仍有可解释 unresolved 为 `Partial`；
没有可执行产出为 `Unavailable`。三者是业务结果，退出成功不等于 `Complete`。技术错误、
状态不一致或提交结果未知使用更强失败语义。

模型响应拒绝作为 task unresolved 记录；每个译前 Placeholder 投影失败单元保留独立的
结构化诊断事实，包含逻辑位置和专用原因，不伪造 task ID、模型 attempt 或响应拒绝原因。
两者都计入最终 `remaining_decisions` 与 `remaining_locations`；因此即使所有已发送任务
都完成，只要仍有 planning-unresolved，本轮也不能解释为全部 Current。普通项目日志可
按稳定 code 和类型化 payload 记录摘要，但日志缺失不改变这些业务事实或提交结果。

翻译规划建立真实任务总数后，进度显示“已确认任务 `x/N`”；只有该任务必要的数据库提交
成功后才推进，`Complete`、`Partial` 与 `Unavailable` 都计入。零任务显示“无需调用模型”，
不显示 `0/0`。到达 `N/N` 后仍进入必要收尾与保存运行方案。

全部业务阶段成功且必要非日志根完成收尾后，最后一个短事务精确替换
`translate_run_plan` 及 Translate Lua 程序。确认提交失败时旧方案保持；终态无法确认时
命令说明翻译结果已生效但方案状态无法确认，并建议下次显式传入 Profile 与 Lua 选择。
项目日志的启动、写入或关闭故障不停止模型任务、不丢弃合法候选，也不改变退出码。
